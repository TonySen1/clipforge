//! Media layer: FFmpeg discovery, probing and keyframe indexing.
//!
//! This module never renders anything and never touches egui. Everything that
//! shells out to FFmpeg lives here so the UI thread can stay free of process
//! I/O.

pub mod audio;
pub mod capture;
pub mod export;
pub mod ffmpeg;
pub mod keyframes;
pub mod preview;
pub mod probe;
pub mod progress;

pub use audio::{AudioBuffer, CHANNELS, SAMPLE_RATE};
pub use capture::{capture_frame, capture_path, ImageFormat, DEFAULT_SUFFIX};
pub use export::{
    check_compat, only_segments, plan_lossless, plan_precise, timeline_pieces, CompatIssue,
    CompatReport, ExportBlocker, ExportMode, ExportRange, LosslessPlan, LosslessRequest,
    PrecisePlan, PreciseSettings, Segment, TimelinePiece, VideoCodecChoice,
};
pub use ffmpeg::{discover, DiscoverySource, EncoderSupport, FfmpegInstall, FfmpegVersion};
pub use keyframes::{KeyframeIndex, SharedKeyframes};
pub use preview::{
    decode_frame, preview_size, Frame, PlaybackProcess, PreviewSize, MAX_PREVIEW_EDGE,
};
pub use probe::{AudioStream, ContainerInfo, MediaInfo, VideoStream};
