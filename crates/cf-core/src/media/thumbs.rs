//! Scrub strip: a coarse frame sequence sampled ahead of time.
//!
//! Scrubbing in ClipForge used to be bounded by one FFmpeg process per seek
//! (~70-100 ms measured). That cost is dominated by process spawn and file
//! open, not by scaling, so lowering the preview resolution does not help.
//!
//! The strip sidesteps it: at import the file is sampled at a few frames per
//! second into memory. While the user drags, the nearest sample is shown at
//! once and the frame-accurate decode lands ~70 ms later.
//!
//! Frames are stored as JPEG. Measured on a 1080p H.264 clip: 720p samples are
//! ~34 KB each, so an 8 fps strip of a 27 s clip is 7 MB, and decoding one
//! takes a few milliseconds. Raw RGBA would have been 25x larger for the same
//! count, which is why the first version had to run at 320 px and looked
//! blurry when scaled up.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::preview::{preview_size, video_filter, Frame};
use crate::media::probe::MediaInfo;
use crate::time::Time;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

/// Longest edge of a strip frame.
///
/// Matches the default preview height, so at the usual quality setting the
/// stand-in is pixel-sharp rather than visibly upscaled.
pub const STRIP_MAX_EDGE: u32 = 720;

/// Memory ceiling for one source's strip.
pub const STRIP_BUDGET_BYTES: usize = 24 * 1024 * 1024;

/// Upper bound on samples per source; keeps long files cheap.
pub const STRIP_MAX_FRAMES: usize = 700;

/// Highest sampling rate. Eight frames a second keeps the stand-in within
/// ~125 ms of the pointer, which reads as the right picture rather than "some
/// other frame".
const STRIP_MAX_FPS: f64 = 8.0;

/// Lowest sampling rate. Only reached by very long files, where the frame cap
/// is what keeps the strip small; one sample every ~20 s still beats a blank.
const STRIP_MIN_FPS: f64 = 0.05;

/// Sampled frames for one source, ordered by time.
#[derive(Clone, Debug, Default)]
pub struct KeyframeStrip {
    times: Vec<Time>,
    /// JPEG bytes, same length as `times`.
    images: Vec<Vec<u8>>,
    width: u32,
    height: u32,
}

impl KeyframeStrip {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn time_at(&self, index: usize) -> Option<Time> {
        self.times.get(index).copied()
    }

    /// Index of the sample at or before `t`, falling back to the first one.
    pub fn index_at_or_before(&self, t: Time) -> Option<usize> {
        if self.times.is_empty() {
            return None;
        }
        Some(match self.times.partition_point(|&time| time <= t) {
            0 => 0,
            n => n - 1,
        })
    }

    /// Decodes one sample to RGBA.
    pub fn decode(&self, index: usize) -> Result<Frame, CoreError> {
        let bytes = self
            .images
            .get(index)
            .ok_or_else(|| CoreError::FfmpegBadOutput {
                program: "strip".to_owned(),
                detail: format!("no sample at index {index}"),
            })?;
        let image = image::load_from_memory(bytes)
            .map_err(|err| CoreError::FfmpegBadOutput {
                program: "strip".to_owned(),
                detail: format!("decoding sample {index}: {err}"),
            })?
            .to_rgba8();
        let (width, height) = image.dimensions();
        Ok(Frame {
            width,
            height,
            rgba: image.into_raw(),
            time: self.times[index],
        })
    }

    /// Builds a strip from already-encoded samples.
    pub fn from_parts(times: Vec<Time>, images: Vec<Vec<u8>>, width: u32, height: u32) -> Self {
        let len = times.len().min(images.len());
        let mut times = times;
        let mut images = images;
        times.truncate(len);
        images.truncate(len);
        Self {
            times,
            images,
            width,
            height,
        }
    }
}

/// Samples per second for a clip of `duration_secs`.
///
/// Short clips get the full rate; long ones are spread over the frame cap so a
/// two-hour film does not try to hold half a million samples.
pub fn sample_rate(duration_secs: f64) -> f64 {
    if duration_secs <= 0.0 {
        return STRIP_MAX_FPS;
    }
    let by_cap = STRIP_MAX_FRAMES as f64 / duration_secs;
    by_cap.clamp(STRIP_MIN_FPS, STRIP_MAX_FPS)
}

/// How many samples to keep for a strip, given a byte budget.
pub fn stride_for(count: usize, frame_bytes: usize, budget: usize) -> usize {
    if count == 0 || frame_bytes == 0 || budget == 0 {
        return 1;
    }
    let max_frames = (budget / frame_bytes).max(1);
    if count <= max_frames {
        1
    } else {
        count.div_ceil(max_frames)
    }
}

/// Samples `path` into a strip.
///
/// One FFmpeg pass with `fps=` resampling, encoded straight to JPEG so nothing
/// larger than the strip ever exists in memory.
pub fn build(
    install: &FfmpegInstall,
    path: &Path,
    info: &MediaInfo,
    max_edge: u32,
    budget: usize,
) -> Result<KeyframeStrip, CoreError> {
    let duration_secs = info.duration.as_secs_f64();
    if duration_secs <= 0.0 {
        return Ok(KeyframeStrip::default());
    }

    let size = preview_size(info, max_edge.min(STRIP_MAX_EDGE));
    if size.width == 0 || size.height == 0 {
        return Ok(KeyframeStrip::default());
    }

    let rate = sample_rate(duration_secs);
    let filter = format!("fps={rate:.3},{}", video_filter(info, size));

    let mut child = hidden_command(&install.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &filter,
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            // q=3 is visually clean for a full-size frame and still compresses
            // to ~34 KB at 720p.
            "-q:v",
            "3",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: install.ffmpeg.clone(),
            source,
        })?;

    let Some(mut stdout) = child.stdout.take() else {
        return Err(CoreError::FfmpegBadOutput {
            program: "ffmpeg".to_owned(),
            detail: "scrub strip has no stdout".to_owned(),
        });
    };

    // Bounded read: a runaway encoder must not exhaust memory.
    let mut encoded = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                encoded.extend_from_slice(&chunk[..n]);
                if encoded.len() > budget * 2 {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    drop(stdout);
    let _ = child.wait();

    let images = split_jpegs(&encoded);
    if images.is_empty() {
        return Ok(KeyframeStrip::default());
    }

    let count = images.len();
    let total_bytes: usize = images.iter().map(Vec::len).sum();
    let stride = stride_for(count, total_bytes / count.max(1), budget);

    let mut times = Vec::with_capacity(count / stride + 1);
    let mut kept = Vec::with_capacity(count / stride + 1);
    for (index, image) in images.into_iter().enumerate() {
        if !index.is_multiple_of(stride) {
            continue;
        }
        // The filters resample uniformly, so sample i sits at i / rate.
        times.push(Time::from_secs(index as f64 / rate));
        kept.push(image);
    }

    Ok(KeyframeStrip {
        times,
        images: kept,
        width: size.width,
        height: size.height,
    })
}

/// Splits a concatenated MJPEG stream into individual JPEGs.
///
/// `image2pipe` emits bare concatenated JPEGs with no length prefix, so the
/// markers are the only framing available: SOI (`FF D8`) starts a sample and
/// EOI (`FF D9`) ends it.
fn split_jpegs(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = None;

    for index in 0..bytes.len().saturating_sub(1) {
        match (bytes[index], bytes[index + 1]) {
            (0xFF, 0xD8) => start = Some(index),
            (0xFF, 0xD9) => {
                if let Some(begin) = start.take() {
                    out.push(bytes[begin..index + 2].to_vec());
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(time_secs: f64) -> (Time, Vec<u8>) {
        // Any bytes work for the indexing tests; decoding is not exercised.
        (Time::from_secs(time_secs), vec![1, 2, 3])
    }

    fn strip(times: &[f64]) -> KeyframeStrip {
        let (times, images): (Vec<_>, Vec<_>) = times.iter().map(|t| sample(*t)).unzip();
        KeyframeStrip::from_parts(times, images, 2, 2)
    }

    #[test]
    fn picks_the_sample_at_or_before() {
        let strip = strip(&[0.0, 0.5, 1.0]);
        assert_eq!(strip.index_at_or_before(Time::from_secs(0.7)), Some(1));
        assert_eq!(strip.index_at_or_before(Time::from_secs(1.0)), Some(2));
        assert_eq!(strip.index_at_or_before(Time::from_secs(9.0)), Some(2));
    }

    #[test]
    fn before_the_first_sample_uses_the_first() {
        let strip = strip(&[1.0, 2.0]);
        assert_eq!(strip.index_at_or_before(Time::ZERO), Some(0));
    }

    #[test]
    fn empty_strip_returns_none() {
        let empty = KeyframeStrip::default();
        assert!(empty.is_empty());
        assert_eq!(empty.index_at_or_before(Time::from_secs(5.0)), None);
    }

    #[test]
    fn sample_rate_caps_long_clips() {
        // A 27 s clip gets the full rate.
        assert_eq!(sample_rate(27.0), STRIP_MAX_FPS);
        // A two-hour clip is spread over the frame cap instead.
        let rate = sample_rate(7200.0);
        assert!((STRIP_MIN_FPS..0.2).contains(&rate));
        assert!(rate * 7200.0 <= STRIP_MAX_FRAMES as f64);
    }

    #[test]
    fn stride_is_one_when_everything_fits() {
        assert_eq!(stride_for(10, 100, 10_000), 1);
        assert_eq!(stride_for(10, 100, 1000), 1);
    }

    #[test]
    fn stride_thins_out_when_over_budget() {
        assert_eq!(stride_for(100, 100, 1000), 10);
        assert_eq!(stride_for(50, 1000, 100), 50);
    }

    #[test]
    fn splits_concatenated_jpegs() {
        let mut stream = Vec::new();
        for payload in [vec![0xAAu8, 0xBB], vec![0xCC]] {
            stream.extend_from_slice(&[0xFF, 0xD8]);
            stream.extend_from_slice(&payload);
            stream.extend_from_slice(&[0xFF, 0xD9]);
        }
        let parts = split_jpegs(&stream);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], vec![0xFF, 0xD8, 0xAA, 0xBB, 0xFF, 0xD9]);
        assert_eq!(parts[1], vec![0xFF, 0xD8, 0xCC, 0xFF, 0xD9]);
    }

    #[test]
    fn ignores_trailing_partial_jpeg() {
        let stream = vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9, 0xFF, 0xD8, 0x02];
        assert_eq!(split_jpegs(&stream).len(), 1);
    }

    #[test]
    fn from_parts_truncates_mismatched_lists() {
        let strip = KeyframeStrip::from_parts(
            vec![Time::ZERO, Time::from_secs(1.0), Time::from_secs(2.0)],
            vec![vec![1], vec![2]],
            2,
            2,
        );
        assert_eq!(strip.len(), 2);
    }
}
