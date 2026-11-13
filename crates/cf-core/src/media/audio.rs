//! Audio decoding for preview playback.
//!
//! Preview audio is short-lived and local, so the whole requested span is
//! decoded to memory up front rather than streamed. Decoding is ~50x realtime
//! on this machine, so a few seconds of audio costs a couple of milliseconds —
//! far cheaper than the synchronisation machinery a streaming decoder needs.
//!
//! Everything is normalised to 48 kHz stereo f32 interleaved, which is what the
//! output device is asked for. Converting once here means the audio callback
//! does nothing but copy samples and advance a cursor.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::time::Time;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

/// Sample rate every preview buffer is decoded at.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channels every preview buffer is decoded to.
pub const CHANNELS: usize = 2;

/// Decoded PCM for some span of a source.
#[derive(Clone, Debug, Default)]
pub struct AudioBuffer {
    /// Interleaved stereo f32 in `[-1, 1]`.
    pub samples: Vec<f32>,
    /// Timeline position this buffer starts at.
    pub start: Time,
}

impl AudioBuffer {
    pub fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn duration(&self) -> Time {
        Time((self.frames() as i64) * 1_000_000 / SAMPLE_RATE as i64)
    }

    /// Frame index for a timeline position inside this buffer.
    ///
    /// Returns `None` when the position falls outside the decoded span, which
    /// is what lets the caller fall back to silence instead of playing a
    /// stretched or shifted buffer.
    pub fn frame_at(&self, timeline: Time) -> Option<usize> {
        let offset = timeline.saturating_sub(self.start);
        if offset.is_negative() || offset > self.duration() {
            return None;
        }
        let frame = (offset.micros() as i128 * SAMPLE_RATE as i128 / 1_000_000) as usize;
        (frame < self.frames()).then_some(frame)
    }
}

/// Decodes `[start, start + duration)` of a source's first audio stream.
///
/// Returns an empty buffer when the source has no audio, which is a normal
/// case (silent screens, image-like clips) rather than an error.
pub fn decode(
    install: &FfmpegInstall,
    path: &Path,
    start: Time,
    duration: Time,
) -> Result<AudioBuffer, CoreError> {
    if duration.micros() <= 0 {
        return Ok(AudioBuffer {
            samples: Vec::new(),
            start,
        });
    }

    let mut child = hidden_command(&install.ffmpeg)
        .args(["-v", "error", "-nostdin", "-ss"])
        .arg(format!("{:.6}", start.as_secs_f64().max(0.0)))
        .arg("-i")
        .arg(path)
        .arg("-t")
        .arg(format!("{:.6}", duration.as_secs_f64()))
        // Drop video entirely: this is the audio-only pass.
        .args(["-vn", "-sn", "-dn"])
        .args([
            "-acodec",
            "pcm_f32le",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-ac",
            &CHANNELS.to_string(),
            // Mono sources are upmixed by FFmpeg's default layout conversion;
            // `-map ...?` keeps a missing audio stream non-fatal.
            "-map",
            "0:a:0?",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: install.ffmpeg.clone(),
            source,
        })?;

    let mut raw = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        // f32 PCM is 4 bytes per sample; a partial trailing sample cannot be
        // played and would shift every later sample, so drop the remainder.
        stdout
            .read_to_end(&mut raw)
            .map_err(|source| CoreError::io(path, source))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|source| CoreError::io(path, source))?;

    // A missing audio stream exits non-zero on some builds; an empty result is
    // the honest answer either way.
    if raw.is_empty() {
        if !output.status.success() {
            log::debug!(
                "audio decode produced nothing for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(AudioBuffer {
            samples: Vec::new(),
            start,
        });
    }

    // Interpret the bytes as little-endian f32. `as_chunks` avoids an
    // intermediate copy and makes the "whole samples only" rule explicit.
    let (chunks, _remainder) = raw.as_chunks::<4>();
    let samples = chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();

    Ok(AudioBuffer { samples, start })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(start_secs: f64, frames: usize) -> AudioBuffer {
        AudioBuffer {
            samples: vec![0.0; frames * CHANNELS],
            start: Time::from_secs(start_secs),
        }
    }

    #[test]
    fn counts_frames_not_samples() {
        let buf = buffer(0.0, 48_000);
        assert_eq!(buf.frames(), 48_000);
        assert_eq!(buf.duration(), Time::from_secs(1.0));
    }

    #[test]
    fn empty_buffer_reports_nothing() {
        let buf = AudioBuffer::default();
        assert!(buf.is_empty());
        assert_eq!(buf.frames(), 0);
    }

    #[test]
    fn maps_timeline_positions_to_frames() {
        let buf = buffer(10.0, 48_000);
        // Exactly at the start -> frame 0.
        assert_eq!(buf.frame_at(Time::from_secs(10.0)), Some(0));
        // Half a second in -> frame 24000.
        assert_eq!(buf.frame_at(Time::from_secs(10.5)), Some(24_000));
    }

    #[test]
    fn rejects_positions_outside_the_buffer() {
        let buf = buffer(10.0, 48_000);
        assert_eq!(buf.frame_at(Time::from_secs(9.0)), None);
        assert_eq!(buf.frame_at(Time::from_secs(11.0)), None);
        assert_eq!(buf.frame_at(Time::from_secs(20.0)), None);
    }

    #[test]
    fn frame_at_is_monotonic() {
        let buf = buffer(0.0, 96_000);
        let mut previous = 0;
        for tenth in 0..20 {
            let t = Time::from_micros(tenth * 100_000);
            let frame = buf.frame_at(t).unwrap();
            assert!(frame >= previous, "frame went backwards at {t}");
            previous = frame;
        }
    }
}
