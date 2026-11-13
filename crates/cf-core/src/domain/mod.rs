//! Domain model.
//!
//! `Project` is the editable document: sources, tracks, clips, playhead. The
//! undo stack lands in P4 and will snapshot this whole struct, so everything
//! here must stay cheap to clone (large data lives behind `Arc`).

pub mod clip;
pub mod edit;
pub mod media_source;
pub mod project;
pub mod undo;

pub use clip::{Clip, ClipId, Track, TrackId, TrackKind};
pub use edit::{EditError, MergeOutcome, TrimEdge};
pub use media_source::{MediaSource, SourceId};
pub use project::{ClipRef, Project};
pub use undo::History;
