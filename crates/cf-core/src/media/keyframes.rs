//! Keyframe index for the video stream.
//!
//! Lossless cutting can only start on a keyframe, so the export preflight and
//! the timeline both need the exact set of keyframe timestamps. We read them
//! once per source and keep them behind an `Arc` so project snapshots (undo)
//! stay cheap to clone.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::time::Time;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

/// Sorted keyframe timestamps of a source's first video stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyframeIndex {
    times: Vec<Time>,
    /// True when the index could not be read and only `[0, duration]` is known.
    degraded: bool,
}

impl KeyframeIndex {
    pub fn new(mut times: Vec<Time>) -> Self {
        times.sort_unstable();
        times.dedup();
        Self {
            times,
            degraded: false,
        }
    }

    /// An index that only knows about the start of the file. Used when probing
    /// fails so the UI can still show the source with a warning.
    pub fn degraded() -> Self {
        Self {
            times: vec![Time::ZERO],
            degraded: true,
        }
    }

    pub fn as_slice(&self) -> &[Time] {
        &self.times
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Last keyframe at or before `t`.
    pub fn before_or_at(&self, t: Time) -> Option<Time> {
        match self.times.partition_point(|&key| key <= t) {
            0 => None,
            n => Some(self.times[n - 1]),
        }
    }

    /// First keyframe at or after `t`.
    pub fn at_or_after(&self, t: Time) -> Option<Time> {
        self.times
            .get(self.times.partition_point(|&key| key < t))
            .copied()
    }

    /// Nearest keyframe to `t`, preferring the earlier one on a tie.
    pub fn nearest(&self, t: Time) -> Option<Time> {
        let before = self.before_or_at(t);
        let after = self.at_or_after(t);
        match (before, after) {
            (Some(b), Some(a)) => {
                if (t.micros() - b.micros()) <= (a.micros() - t.micros()) {
                    Some(b)
                } else {
                    Some(a)
                }
            }
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// Expands a range outward to whole keyframe boundaries.
    ///
    /// This is what a lossless cut will actually produce, so the export dialog
    /// uses it to show the real in/out points and the resulting drift.
    pub fn snap_range(&self, start: Time, end: Time) -> Option<(Time, Time)> {
        let snapped_start = self.before_or_at(start)?;
        let snapped_end = self
            .at_or_after(end)
            .unwrap_or_else(|| self.times.last().copied().unwrap_or(end));
        (snapped_start < snapped_end).then_some((snapped_start, snapped_end))
    }
}

/// Shared, cheaply clonable handle used by `MediaSource`.
pub type SharedKeyframes = Arc<KeyframeIndex>;

/// Reads keyframe timestamps with `ffprobe`.
///
/// Uses `-skip_frame nokey`, which makes the decoder skip non-keyframes: on a
/// 2-hour file this is far cheaper than decoding everything and filtering.
pub fn probe(install: &FfmpegInstall, path: &Path) -> Result<KeyframeIndex, CoreError> {
    let output = hidden_command(&install.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: install.ffprobe.clone(),
            source,
        })?;

    if !output.status.success() {
        return Err(CoreError::FfmpegFailed {
            program: install.ffprobe.display().to_string(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(parse(&output.stdout))
}

/// Parses `csv=p=0` output where each line may be `pts_time` or
/// `pts_time,` (ffprobe appends a trailing comma when extra fields exist).
pub fn parse(stdout: &[u8]) -> KeyframeIndex {
    let text = String::from_utf8_lossy(stdout);
    let times = text
        .lines()
        .filter_map(|line| {
            let field = line.split(',').next()?.trim();
            // `N/A` appears for streams with no timestamps.
            Time::parse_secs(field)
        })
        .collect();

    let mut index = KeyframeIndex::new(times);
    if index.is_empty() {
        index = KeyframeIndex::degraded();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(secs: &[f64]) -> KeyframeIndex {
        KeyframeIndex::new(secs.iter().map(|s| Time::from_secs(*s)).collect())
    }

    #[test]
    fn sorts_and_dedups() {
        let index = KeyframeIndex::new(vec![Time(3_000_000), Time(1_000_000), Time(3_000_000)]);
        assert_eq!(index.as_slice(), &[Time(1_000_000), Time(3_000_000)]);
    }

    #[test]
    fn finds_previous_and_next_keyframes() {
        let index = idx(&[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(
            index.before_or_at(Time::from_secs(1.4)),
            Some(Time::from_secs(1.0))
        );
        assert_eq!(
            index.at_or_after(Time::from_secs(1.4)),
            Some(Time::from_secs(2.0))
        );
        assert_eq!(index.before_or_at(Time::from_secs(0.5)), Some(Time::ZERO));
        assert_eq!(index.at_or_after(Time::from_secs(3.5)), None);
        assert_eq!(index.before_or_at(Time(-1_000_000)), None);
    }

    #[test]
    fn nearest_prefers_earlier_on_tie() {
        let index = idx(&[0.0, 2.0]);
        assert_eq!(index.nearest(Time::from_secs(1.0)), Some(Time::ZERO));
        assert_eq!(
            index.nearest(Time::from_secs(1.6)),
            Some(Time::from_secs(2.0))
        );
    }

    #[test]
    fn snaps_ranges_outward() {
        let index = idx(&[0.0, 1.0, 2.0, 3.0]);
        let (start, end) = index
            .snap_range(Time::from_secs(1.3), Time::from_secs(2.4))
            .unwrap();
        assert_eq!(start, Time::from_secs(1.0));
        assert_eq!(end, Time::from_secs(3.0));
    }

    #[test]
    fn snap_range_expands_into_the_whole_keyframe_interval() {
        let index = idx(&[0.0, 5.0]);
        assert_eq!(
            index.snap_range(Time::from_secs(1.0), Time::from_secs(2.0)),
            Some((Time::ZERO, Time::from_secs(5.0)))
        );
    }

    #[test]
    fn snap_range_returns_none_when_it_collapses() {
        // Both ends snap to the final keyframe, so nothing would be written.
        let index = idx(&[0.0, 5.0]);
        let start = Time::from_secs(5.0);
        let end = Time::from_secs(6.0);
        assert!(index.snap_range(start, end).is_none());
    }

    #[test]
    fn parses_csv_output() {
        let index = parse(b"0.000000\n1.000000,\n2.000000\n");
        assert_eq!(index.len(), 3);
        assert_eq!(index.as_slice()[2], Time::from_secs(2.0));
        assert!(!index.is_degraded());
    }

    #[test]
    fn ignores_na_and_empty_output() {
        assert!(parse(b"N/A\n").is_degraded());
        assert!(parse(b"").is_degraded());
        assert_eq!(parse(b"").len(), 1);
    }
}
