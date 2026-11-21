//! Keyframe thumbnail strip.
//!
//! Scrubbing in ClipForge used to be bounded by one FFmpeg process per seek
//! (~70-100 ms measured). That cost is dominated by process spawn and file
//! open, not by scaling, so lowering the preview resolution does not help.
//!
//! The strip sidesteps it: every keyframe is decoded once, at import, into a
//! small RGBA buffer. While the user drags, the nearest strip frame is shown
//! immediately (a memory read) and the frame-accurate decode lands ~70 ms
//! later to sharpen the picture.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::preview::{preview_size, video_filter, Frame};
use crate::media::probe::MediaInfo;
use crate::media::KeyframeIndex;
use crate::time::Time;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::Stdio;

/// Longest edge of a strip frame. 320 keeps a scrubbing picture recognisable
/// while costing ~225 KB per frame at 16:9.
pub const STRIP_MAX_EDGE: u32 = 320;

/// Memory ceiling for one source's strip.
///
/// A 2-hour file with a 10 s GOP has ~720 keyframes; at 320 px that would be
/// 160 MB, so long files keep every Nth keyframe instead.
pub const STRIP_BUDGET_BYTES: usize = 24 * 1024 * 1024;

/// Decoded keyframes for one source, ordered by time.
#[derive(Clone, Debug, Default)]
pub struct KeyframeStrip {
    times: Vec<Time>,
    frames: Vec<Frame>,
}

impl KeyframeStrip {
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Frame shown while scrubbing to `t`: the nearest keyframe at or before
    /// it, falling back to the first frame when `t` sits before all of them.
    pub fn at_or_before(&self, t: Time) -> Option<&Frame> {
        if self.frames.is_empty() {
            return None;
        }
        let index = match self.times.partition_point(|&key| key <= t) {
            0 => 0,
            n => n - 1,
        };
        self.frames.get(index)
    }

    /// Builds a strip from times and frames that are already aligned.
    pub fn from_parts(times: Vec<Time>, frames: Vec<Frame>) -> Self {
        let len = times.len().min(frames.len());
        let mut times = times;
        let mut frames = frames;
        times.truncate(len);
        frames.truncate(len);
        Self { times, frames }
    }
}

/// How many keyframes to keep for a strip of `size`, given a byte budget.
///
/// Returns the stride: 1 keeps every keyframe, 3 keeps every third.
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

/// Decodes every keyframe of `path` into a strip.
///
/// One FFmpeg pass with `-skip_frame nokey`, which makes the decoder skip
/// non-key frames instead of decoding them and throwing them away. The decoded
/// frames come out in order, so they are paired with `keyframes` by index.
pub fn build(
    install: &FfmpegInstall,
    path: &Path,
    info: &MediaInfo,
    keyframes: &KeyframeIndex,
    max_edge: u32,
    budget: usize,
) -> Result<KeyframeStrip, CoreError> {
    let times = keyframes.as_slice();
    if times.is_empty() || keyframes.is_degraded() {
        return Ok(KeyframeStrip::default());
    }

    let size = preview_size(info, max_edge.min(STRIP_MAX_EDGE));
    if size.width == 0 || size.height == 0 {
        return Ok(KeyframeStrip::default());
    }

    let frame_bytes = size.frame_bytes();
    let stride = stride_for(times.len(), frame_bytes, budget);

    let mut child = hidden_command(&install.ffmpeg)
        .args(["-v", "error", "-nostdin", "-skip_frame", "nokey", "-i"])
        .arg(path)
        .args([
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &video_filter(info, size),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
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

    let Some(stdout) = child.stdout.take() else {
        return Err(CoreError::FfmpegBadOutput {
            program: "ffmpeg".to_owned(),
            detail: "keyframe strip has no stdout".to_owned(),
        });
    };

    let mut reader = BufReader::with_capacity(frame_bytes * 2, stdout);
    let mut kept_times = Vec::new();
    let mut kept_frames = Vec::new();
    let mut buffer = vec![0u8; frame_bytes];
    let mut index = 0usize;
    // Set when we stop reading before FFmpeg finishes writing. Waiting on a
    // process that is still blocked on a full pipe would deadlock, so that
    // case kills it instead.
    let mut stopped_early = false;

    loop {
        match read_frame(&mut reader, &mut buffer) {
            Ok(true) => {}
            // End of stream, or a short final frame.
            Ok(false) => break,
            Err(_) => break,
        }

        let Some(&time) = times.get(index) else {
            // More decoded frames than keyframe timestamps: the two lists
            // disagree, so stop rather than pair them wrongly.
            stopped_early = true;
            break;
        };

        if index.is_multiple_of(stride) {
            kept_times.push(time);
            kept_frames.push(Frame {
                width: size.width,
                height: size.height,
                rgba: buffer.clone(),
                time,
            });
        }
        index += 1;
    }

    drop(reader);
    if stopped_early {
        let _ = child.kill();
    }
    let _ = child.wait();

    Ok(KeyframeStrip {
        times: kept_times,
        frames: kept_frames,
    })
}

/// Fills `buffer` completely. `Ok(false)` means the stream ended.
fn read_frame(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(time_secs: f64, size: u32) -> Frame {
        Frame {
            width: size,
            height: size,
            rgba: vec![0u8; (size * size * 4) as usize],
            time: Time::from_secs(time_secs),
        }
    }

    fn strip(times: &[f64]) -> KeyframeStrip {
        KeyframeStrip::from_parts(
            times.iter().map(|t| Time::from_secs(*t)).collect(),
            times.iter().map(|t| frame(*t, 2)).collect(),
        )
    }

    #[test]
    fn picks_the_keyframe_at_or_before() {
        let strip = strip(&[0.0, 2.0, 4.0]);
        assert_eq!(
            strip.at_or_before(Time::from_secs(3.0)).unwrap().time,
            Time::from_secs(2.0)
        );
        assert_eq!(
            strip.at_or_before(Time::from_secs(2.0)).unwrap().time,
            Time::from_secs(2.0)
        );
        assert_eq!(
            strip.at_or_before(Time::from_secs(9.0)).unwrap().time,
            Time::from_secs(4.0)
        );
    }

    #[test]
    fn before_the_first_keyframe_uses_the_first() {
        let strip = strip(&[1.0, 2.0]);
        assert_eq!(
            strip.at_or_before(Time::ZERO).unwrap().time,
            Time::from_secs(1.0)
        );
    }

    #[test]
    fn empty_strip_returns_none() {
        let empty = KeyframeStrip::default();
        assert!(empty.is_empty());
        assert!(empty.at_or_before(Time::from_secs(5.0)).is_none());
    }

    #[test]
    fn stride_is_one_when_everything_fits() {
        assert_eq!(stride_for(10, 100, 10_000), 1);
        assert_eq!(stride_for(10, 100, 1000), 1);
    }

    #[test]
    fn stride_thins_out_when_over_budget() {
        // 100 frames of 100 bytes into a 1000-byte budget: keep every 10th.
        assert_eq!(stride_for(100, 100, 1000), 10);
        // A budget smaller than one frame still keeps the first.
        assert_eq!(stride_for(50, 1000, 100), 50);
    }

    #[test]
    fn from_parts_truncates_mismatched_lists() {
        let strip = KeyframeStrip::from_parts(
            vec![Time::ZERO, Time::from_secs(1.0), Time::from_secs(2.0)],
            vec![frame(0.0, 2), frame(1.0, 2)],
        );
        assert_eq!(strip.len(), 2);
    }
}
