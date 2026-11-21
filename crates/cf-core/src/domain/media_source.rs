//! Imported media sources.
//!
//! A `MediaSource` is the immutable description of one file on disk. Clips
//! reference sources by id and never mutate them, which is what makes the
//! snapshot-based undo stack cheap: cloning a project clones these handles,
//! not the keyframe vectors behind them.

use crate::media::{KeyframeIndex, KeyframeStrip, MediaInfo, SharedKeyframes};
use crate::time::Time;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Stable identifier, unique within one project.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceId(pub u64);

/// One imported file plus everything derived from it.
#[derive(Clone, Debug)]
pub struct MediaSource {
    pub id: SourceId,
    pub info: Arc<MediaInfo>,
    pub keyframes: SharedKeyframes,
    /// Decoded keyframes for instant scrub feedback. Behind an `Arc` because
    /// the project is cloned on every edit for the undo stack; cloning a few
    /// megabytes of pixels there would be a hidden cost.
    pub thumbs: Option<Arc<KeyframeStrip>>,
    /// Playable length. Currently the container duration, but kept separate so
    /// a future "trim source" feature does not change call sites.
    pub duration: Time,
}

impl MediaSource {
    pub fn new(id: SourceId, info: MediaInfo, keyframes: KeyframeIndex) -> Self {
        let duration = info.duration;
        Self {
            id,
            info: Arc::new(info),
            keyframes: Arc::new(keyframes),
            thumbs: None,
            duration,
        }
    }

    /// Attaches the scrub strip built after import.
    ///
    /// Kept separate from `new` because the strip takes a few hundred
    /// milliseconds; the source is usable (and shown in the list) before it
    /// exists.
    pub fn set_thumbs(&mut self, strip: KeyframeStrip) {
        self.thumbs = Some(Arc::new(strip));
    }

    pub fn thumbs(&self) -> Option<&KeyframeStrip> {
        self.thumbs.as_deref()
    }

    /// Fallback used when keyframe probing failed: the source stays usable for
    /// preview but lossless export will report a problem.
    pub fn with_degraded_keyframes(id: SourceId, info: MediaInfo) -> Self {
        Self::new(id, info, KeyframeIndex::degraded())
    }

    pub fn path(&self) -> &PathBuf {
        &self.info.path
    }

    /// File name for display, falling back to the full path.
    pub fn display_name(&self) -> String {
        self.info
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.info.path.display().to_string())
    }

    pub fn has_audio(&self) -> bool {
        self.info.has_audio()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe;

    fn sample_info() -> MediaInfo {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        probe::parse(std::path::Path::new("sample_h264.mp4"), &json).unwrap()
    }

    #[test]
    fn exposes_duration_and_name() {
        let source = MediaSource::new(
            SourceId(1),
            sample_info(),
            KeyframeIndex::new(vec![Time::ZERO]),
        );
        assert_eq!(source.duration, Time::from_secs(4.0));
        assert_eq!(source.display_name(), "sample_h264.mp4");
        assert!(source.has_audio());
        assert!(!source.keyframes.is_degraded());
    }

    #[test]
    fn degraded_keyframes_keep_the_source_usable() {
        let source = MediaSource::with_degraded_keyframes(SourceId(2), sample_info());
        assert!(source.keyframes.is_degraded());
        assert_eq!(source.duration, Time::from_secs(4.0));
    }
}
