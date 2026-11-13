//! Export pipeline.
//!
//! Two modes, sharing one preflight:
//!
//! * **Lossless** — `-c copy`. Fast and bit-exact, but cuts can only land on
//!   keyframes and every segment must share codec parameters before they can be
//!   concatenated.
//! * **Precise** — re-encodes the timeline through a filter graph. Cuts land
//!   exactly where the user put them and gaps become black/silence, at the cost
//!   of generation loss and time.
//!
//! Everything here builds commands and validates inputs. Running them is the
//! app layer's job (`cf-app/src/export.rs`), so this module stays testable
//! without spawning processes.

pub mod compat;
pub mod lossless;
pub mod precise;

pub use compat::{check_compat, CompatIssue, CompatReport};
pub use lossless::{
    build_lossless_command, container_for, finalize_output, plan_lossless, resolve, same_file,
    validate_output, LosslessPlan, LosslessRequest,
};
pub use precise::{
    build_precise_command, plan_precise, PrecisePlan, PreciseSettings, VideoCodecChoice,
};

use crate::domain::Project;
use crate::time::Time;
use std::path::Path;

/// Why an export cannot run. Shared by both modes so the UI has one match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportBlocker {
    /// The keyframe index never loaded, so lossless cut points are unknown.
    KeyframesUnavailable,
    /// Snapping collapsed the range to nothing.
    EmptyRange,
    /// The output path is the same as an input path.
    OutputIsInput,
    /// FFmpeg is not available.
    EngineMissing,
    /// The clip references a source that is no longer in the project.
    MissingSource,
    /// Nothing is placed on the video track.
    NothingToExport,
    /// The timeline contains gaps, which a stream copy cannot represent.
    TimelineHasGaps,
    /// Two clips cannot be concatenated without re-encoding.
    IncompatibleSegments(CompatReport),
    /// The requested output container cannot hold the chosen codecs.
    UnsupportedContainer(String),
    /// The selected range does not intersect any clip.
    RangeEmpty,
}

impl ExportBlocker {
    pub fn i18n_key(&self) -> &'static str {
        match self {
            Self::KeyframesUnavailable => "export.blocker.keyframes",
            Self::EmptyRange => "export.blocker.empty_range",
            Self::OutputIsInput => "export.blocker.output_is_input",
            Self::EngineMissing => "export.blocker.engine_missing",
            Self::MissingSource => "export.blocker.missing_source",
            Self::NothingToExport => "export.blocker.nothing",
            Self::TimelineHasGaps => "export.blocker.gaps",
            Self::IncompatibleSegments(_) => "export.blocker.incompatible",
            Self::UnsupportedContainer(_) => "export.blocker.container",
            Self::RangeEmpty => "export.blocker.range_empty",
        }
    }

    /// Extra detail lines for the dialog; empty for self-explanatory blockers.
    pub fn details(&self) -> Vec<String> {
        match self {
            Self::IncompatibleSegments(report) => {
                report.issues.iter().map(|issue| issue.describe()).collect()
            }
            Self::UnsupportedContainer(name) => vec![name.clone()],
            _ => Vec::new(),
        }
    }
}

/// What the user chose in the export dialog.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ExportMode {
    /// Stream copy, keyframe-snapped.
    #[default]
    Lossless,
    /// Re-encode for exact cut points and gap filling.
    Precise,
}

impl ExportMode {
    pub const ALL: [Self; 2] = [Self::Lossless, Self::Precise];

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Lossless => "export.mode_lossless",
            Self::Precise => "export.mode_precise",
        }
    }

    pub fn hint_key(self) -> &'static str {
        match self {
            Self::Lossless => "export.mode_lossless_hint",
            Self::Precise => "export.mode_precise_hint",
        }
    }
}

/// Which part of the timeline to write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExportRange {
    /// Everything on the video track.
    #[default]
    Timeline,
    /// One clip's own span.
    Clip(crate::ClipId),
    /// An explicit timeline window.
    Window { start: Time, end: Time },
}

impl ExportRange {
    /// Resolves the range to an absolute timeline window.
    pub fn resolve(&self, project: &Project) -> Result<(Time, Time), ExportBlocker> {
        match self {
            Self::Timeline => {
                let end = project.duration();
                if end <= Time::ZERO {
                    return Err(ExportBlocker::NothingToExport);
                }
                Ok((Time::ZERO, end))
            }
            Self::Clip(id) => {
                let entry = project.clip(*id).ok_or(ExportBlocker::MissingSource)?;
                Ok((entry.clip.timeline_start, entry.clip.timeline_end()))
            }
            Self::Window { start, end } => {
                if *end <= *start {
                    return Err(ExportBlocker::RangeEmpty);
                }
                Ok((*start, *end))
            }
        }
    }
}

/// One contiguous piece of the timeline that maps to one source section.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub source_id: crate::SourceId,
    pub source_in: Time,
    pub source_out: Time,
    /// Where this piece sits on the timeline.
    pub timeline_start: Time,
    pub timeline_end: Time,
}

impl Segment {
    pub fn duration(&self) -> Time {
        self.source_out.saturating_sub(self.source_in)
    }
}

/// One entry in the export order: either a real segment or a gap.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelinePiece {
    Clip(Segment),
    /// Empty timeline span, with its own extent so gap-filling knows how long
    /// to synthesise.
    Gap {
        start: Time,
        end: Time,
    },
}

impl TimelinePiece {
    pub fn is_gap(&self) -> bool {
        matches!(self, Self::Gap { .. })
    }

    pub fn duration(&self) -> Time {
        match self {
            Self::Clip(segment) => segment.duration(),
            Self::Gap { start, end } => end.saturating_sub(*start),
        }
    }
}

/// Cuts the timeline window into the ordered pieces an export needs.
///
/// Gaps are explicit, with their own extent, so callers can decide whether to
/// refuse them (lossless) or fill them (precise).
pub fn timeline_pieces(
    project: &Project,
    start: Time,
    end: Time,
) -> Result<Vec<TimelinePiece>, ExportBlocker> {
    let track = project
        .video_track()
        .ok_or(ExportBlocker::NothingToExport)?;

    if track.clips.is_empty() {
        return Err(ExportBlocker::NothingToExport);
    }

    let mut out = Vec::new();
    let mut cursor = start;

    while cursor < end {
        match track.clip_at(cursor) {
            Some(clip) => {
                let segment_end = clip.timeline_end().min(end);
                out.push(TimelinePiece::Clip(Segment {
                    source_id: clip.source_id,
                    source_in: clip.source_time_at(cursor),
                    source_out: clip.source_time_at(segment_end),
                    timeline_start: cursor,
                    timeline_end: segment_end,
                }));
                cursor = segment_end;
            }
            None => {
                // Walk forward to the next clip boundary or the window end.
                let next = track
                    .clips
                    .iter()
                    .find(|clip| clip.timeline_start > cursor)
                    .map(|clip| clip.timeline_start.min(end))
                    .unwrap_or(end)
                    .max(cursor); // never stand still

                let gap_end = next.max(cursor);
                if gap_end > cursor {
                    out.push(TimelinePiece::Gap {
                        start: cursor,
                        end: gap_end,
                    });
                    cursor = gap_end;
                } else {
                    // Nothing to the right: the rest of the window is a gap.
                    out.push(TimelinePiece::Gap { start: cursor, end });
                    cursor = end;
                }
            }
        }
    }

    if out.is_empty() {
        return Err(ExportBlocker::RangeEmpty);
    }
    Ok(out)
}

/// True when the piece list contains a gap.
pub fn has_gaps(pieces: &[TimelinePiece]) -> bool {
    pieces.iter().any(TimelinePiece::is_gap)
}

/// Convenience: keeps only the real segments, in export order.
pub fn only_segments(pieces: &[TimelinePiece]) -> Vec<Segment> {
    pieces
        .iter()
        .filter_map(|piece| match piece {
            TimelinePiece::Clip(segment) => Some(segment.clone()),
            TimelinePiece::Gap { .. } => None,
        })
        .collect()
}

/// Formats a drift value for the UI, e.g. `-1.500s` or `0`.
pub fn format_drift(drift: Time) -> String {
    if drift.micros() == 0 {
        return "0".to_owned();
    }
    format!("{:+.3}s", drift.as_secs_f64())
}

/// Sibling `.part` path used while an export runs.
pub fn temp_path(output: &Path) -> std::path::PathBuf {
    let mut temp = output.to_path_buf();
    let name = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_owned());
    temp.set_file_name(format!(".{name}.part"));
    temp
}

/// Removes the temp file, ignoring "already gone".
pub fn cleanup_temp(temp: &Path) {
    if temp.exists() {
        if let Err(err) = std::fs::remove_file(temp) {
            log::warn!("could not remove {}: {err}", temp.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Clip, ClipId, MediaSource, SourceId};

    fn project_with(lengths: &[f64], gap_before: &[f64]) -> Project {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();

        let mut project = Project::empty();
        for (index, len) in lengths.iter().enumerate() {
            let id = project.alloc_source_id();
            let mut info =
                crate::media::probe::parse(std::path::Path::new(&format!("a{index}.mp4")), &json)
                    .unwrap();
            info.duration = Time::from_secs(*len);
            let mut source = MediaSource::new(
                id,
                info,
                crate::media::KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(*len)]),
            );
            source.id = id;
            let clip_id = project.add_source(source);

            if let Some(gap) = gap_before.get(index) {
                if *gap > 0.0 {
                    project.move_clip(clip_id, Time::from_secs(*gap)).unwrap();
                }
            }
        }
        project
    }

    #[test]
    fn whole_timeline_is_one_segment() {
        let project = project_with(&[4.0], &[]);
        let pieces = timeline_pieces(&project, Time::ZERO, Time::from_secs(4.0)).unwrap();
        assert_eq!(pieces.len(), 1);
        let TimelinePiece::Clip(segment) = &pieces[0] else {
            panic!("expected a clip");
        };
        assert_eq!(segment.source_in, Time::ZERO);
        assert_eq!(segment.source_out, Time::from_secs(4.0));
        assert!(!has_gaps(&pieces));
    }

    #[test]
    fn adjacent_clips_become_two_segments() {
        let project = project_with(&[4.0, 3.0], &[]);
        let pieces = timeline_pieces(&project, Time::ZERO, Time::from_secs(7.0)).unwrap();
        assert_eq!(pieces.len(), 2);
        assert!(!has_gaps(&pieces));
        assert_eq!(pieces[0].duration(), Time::from_secs(4.0));
        assert_eq!(pieces[1].duration(), Time::from_secs(3.0));
    }

    #[test]
    fn a_gap_becomes_none() {
        let project = project_with(&[3.0, 3.0], &[0.0, 5.0]);
        let pieces = timeline_pieces(&project, Time::ZERO, Time::from_secs(8.0)).unwrap();

        // clip1 0-3 s, gap 3-5 s, clip2 5-8 s.
        assert_eq!(pieces.len(), 3);
        assert!(!pieces[0].is_gap());
        assert!(pieces[1].is_gap(), "3-5 s is empty");
        assert!(!pieces[2].is_gap());
        assert_eq!(pieces[1].duration(), Time::from_secs(2.0));
        assert!(has_gaps(&pieces));
    }

    #[test]
    fn window_clips_partial_segments() {
        let project = project_with(&[10.0], &[]);
        let pieces = timeline_pieces(&project, Time::from_secs(2.0), Time::from_secs(5.0)).unwrap();

        assert_eq!(pieces.len(), 1);
        let TimelinePiece::Clip(segment) = &pieces[0] else {
            panic!("expected a clip");
        };
        assert_eq!(segment.source_in, Time::from_secs(2.0));
        assert_eq!(segment.source_out, Time::from_secs(5.0));
    }

    #[test]
    fn empty_project_has_nothing_to_export() {
        let project = Project::empty();
        assert_eq!(
            timeline_pieces(&project, Time::ZERO, Time::from_secs(1.0)),
            Err(ExportBlocker::NothingToExport)
        );
    }

    #[test]
    fn empty_window_is_refused() {
        let project = project_with(&[4.0], &[]);
        assert_eq!(
            timeline_pieces(&project, Time::from_secs(2.0), Time::from_secs(2.0)),
            Err(ExportBlocker::RangeEmpty)
        );
    }

    #[test]
    fn ranges_resolve_against_the_project() {
        let project = project_with(&[4.0, 3.0], &[]);

        assert_eq!(
            ExportRange::Timeline.resolve(&project),
            Ok((Time::ZERO, Time::from_secs(7.0)))
        );

        let first = project.video_track().unwrap().clips[0].id;
        assert_eq!(
            ExportRange::Clip(first).resolve(&project),
            Ok((Time::ZERO, Time::from_secs(4.0)))
        );

        assert_eq!(
            ExportRange::Window {
                start: Time::from_secs(1.0),
                end: Time::from_secs(2.0)
            }
            .resolve(&project),
            Ok((Time::from_secs(1.0), Time::from_secs(2.0)))
        );

        assert_eq!(
            ExportRange::Clip(ClipId(999)).resolve(&project),
            Err(ExportBlocker::MissingSource)
        );
    }

    #[test]
    fn segment_maps_source_time_offset() {
        let clip = Clip {
            id: ClipId(1),
            source_id: SourceId(1),
            source_in: Time::from_secs(2.0),
            source_out: Time::from_secs(6.0),
            timeline_start: Time::from_secs(10.0),
            group: None,
            name: "x".into(),
        };
        assert_eq!(
            clip.source_time_at(Time::from_secs(11.0)),
            Time::from_secs(3.0)
        );
        assert_eq!(
            clip.source_time_at(Time::from_secs(10.0)),
            Time::from_secs(2.0)
        );
        assert_eq!(
            clip.source_time_at(Time::from_secs(16.0)),
            Time::from_secs(6.0)
        );
    }
}
