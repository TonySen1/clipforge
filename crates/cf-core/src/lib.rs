//! ClipForge core: domain model, time math, media pipeline and preferences.
//!
//! This crate must stay UI-agnostic: no egui/eframe types, no user-facing
//! strings. Errors expose structured kinds that the UI localizes.

pub mod domain;
pub mod error;
pub mod media;
pub mod prefs;
pub mod time;
pub mod version;

pub use domain::{
    Clip, ClipId, EditError, History, MediaSource, MergeOutcome, Project, SourceId, Track, TrackId,
    TrackKind, TrimEdge,
};
pub use error::CoreError;
pub use media::{FfmpegInstall, KeyframeIndex, MediaInfo};
pub use prefs::{CaptureFormat, Locale, Preferences, PreviewQuality, ThemeMode};
pub use time::{FrameRate, Time, TimeRange};
