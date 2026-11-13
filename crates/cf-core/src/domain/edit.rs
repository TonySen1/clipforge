//! Editing operations on the timeline.
//!
//! Every function here is pure and fallible: it either produces a valid
//! timeline or explains why it cannot. Callers never get a half-applied edit,
//! which is what makes the undo stack trivial — it snapshots `Project` before
//! and after a call that returned `Ok`.
//!
//! Invariants preserved by every operation:
//!
//! * clips on a track stay ordered by `timeline_start` and never overlap;
//! * `0 <= source_in < source_out <= source duration`;
//! * a clip is always at least one frame long;
//! * `timeline_start >= 0`;
//! * clips never reference a missing source.

use crate::domain::clip::{Clip, ClipId};
use crate::domain::project::Project;
use crate::time::{FrameRate, Time};
use std::fmt;

/// Why an edit was refused. The UI turns these into localized messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The referenced clip is not in the project.
    NotFound(ClipId),
    /// The cut point is not strictly inside the clip.
    SplitOutsideClip { clip: ClipId, at: Time },
    /// The result would be shorter than one frame.
    TooShort { clip: ClipId, min: Time },
    /// The clip would run past the end of its source.
    PastSourceEnd {
        clip: ClipId,
        attempted: Time,
        source_duration: Time,
    },
    /// The clip would start before the beginning of its source.
    BeforeSourceStart { clip: ClipId },
    /// The target position would overlap another clip on the track.
    Overlap { clip: ClipId, other: ClipId },
    /// The target position would place the clip before time zero.
    NegativeStart { clip: ClipId },
    /// Merge needs two clips that actually touch.
    NotAdjacent { left: ClipId, right: ClipId },
    /// Merge needs two clips from the same source and contiguous source range.
    NotMergeable { left: ClipId, right: ClipId },
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "clip {} not found", id.0),
            Self::SplitOutsideClip { clip, at } => {
                write!(f, "cannot split clip {} at {at}", clip.0)
            }
            Self::TooShort { clip, min } => {
                write!(f, "clip {} would be shorter than {min}", clip.0)
            }
            Self::PastSourceEnd {
                clip,
                attempted,
                source_duration,
            } => write!(
                f,
                "clip {} would end at {attempted} but its source ends at {source_duration}",
                clip.0
            ),
            Self::BeforeSourceStart { clip } => {
                write!(f, "clip {} would start before its source", clip.0)
            }
            Self::Overlap { clip, other } => {
                write!(f, "clip {} would overlap clip {}", clip.0, other.0)
            }
            Self::NegativeStart { clip } => {
                write!(f, "clip {} cannot start before zero", clip.0)
            }
            Self::NotAdjacent { left, right } => {
                write!(f, "clips {} and {} do not touch", left.0, right.0)
            }
            Self::NotMergeable { left, right } => {
                write!(f, "clips {} and {} cannot merge", left.0, right.0)
            }
        }
    }
}

/// Minimum clip length: one frame at the clip's own frame rate.
fn min_length(rate: FrameRate) -> Time {
    rate.frame_duration().max(Time(1_000))
}

/// Result of a successful merge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Same source and contiguous source range: the two clips became one.
    Merged { kept: ClipId, removed: ClipId },
    /// Different source or non-contiguous range: both clips survive but now
    /// share a group id, so they select and move together.
    Grouped { group: u64 },
}

impl Project {
    /// Splits a clip at a timeline position, producing two clips that together
    /// cover exactly the original range.
    ///
    /// The left half keeps the original id so selections survive the split.
    pub fn split_clip(
        &mut self,
        clip_id: ClipId,
        at: Time,
        rate: FrameRate,
    ) -> Result<(), EditError> {
        let (track_index, position) = self
            .locate_clip(clip_id)
            .ok_or(EditError::NotFound(clip_id))?;

        let original = self.tracks[track_index].clips[position].clone();
        let min = min_length(rate);

        if at <= original.timeline_start || at >= original.timeline_end() {
            return Err(EditError::SplitOutsideClip { clip: clip_id, at });
        }

        let source_cut = original.source_time_at(at);
        let left_len = at.saturating_sub(original.timeline_start);
        let right_len = original.timeline_end().saturating_sub(at);

        if left_len < min || right_len < min {
            return Err(EditError::TooShort { clip: clip_id, min });
        }

        let right_id = self.alloc_clip_id();
        let mut left = original.clone();
        left.source_out = source_cut;

        let right = Clip {
            id: right_id,
            source_id: original.source_id,
            source_in: source_cut,
            source_out: original.source_out,
            timeline_start: at,
            group: original.group,
            name: original.name.clone(),
        };

        self.tracks[track_index].clips[position] = left;
        self.tracks[track_index].clips.insert(position + 1, right);
        Ok(())
    }

    /// Moves a clip so it starts at `new_start`, keeping its duration.
    pub fn move_clip(&mut self, clip_id: ClipId, new_start: Time) -> Result<(), EditError> {
        if new_start < Time::ZERO {
            return Err(EditError::NegativeStart { clip: clip_id });
        }

        let (track_index, position) = self
            .locate_clip(clip_id)
            .ok_or(EditError::NotFound(clip_id))?;

        let mut clip = self.tracks[track_index].clips[position].clone();
        clip.timeline_start = new_start;

        // Check against everything except itself before mutating.
        if let Some(other) = self.find_overlap(track_index, &clip) {
            return Err(EditError::Overlap {
                clip: clip_id,
                other,
            });
        }

        self.tracks[track_index].clips.remove(position);
        let insert_at = self.tracks[track_index]
            .clips
            .partition_point(|existing| existing.timeline_start <= clip.timeline_start);
        self.tracks[track_index].clips.insert(insert_at, clip);

        // Keep moving any grouped siblings so a "cross-source merge" behaves
        // as one object.
        Ok(())
    }

    /// Moves a clip and every clip sharing its group by the same delta.
    pub fn move_group(&mut self, clip_id: ClipId, new_start: Time) -> Result<(), EditError> {
        let Some(entry) = self.clip(clip_id) else {
            return Err(EditError::NotFound(clip_id));
        };
        let Some(group) = entry.clip.group else {
            return self.move_clip(clip_id, new_start);
        };

        let delta = new_start.saturating_sub(entry.clip.timeline_start);
        let members: Vec<ClipId> = self
            .clips()
            .filter(|e| e.clip.group == Some(group))
            .map(|e| e.clip.id)
            .collect();

        // Validate every member first so a partial move is impossible.
        for member in &members {
            let Some(entry) = self.clip(*member) else {
                continue;
            };
            let target = entry.clip.timeline_start.saturating_add(delta);
            if target < Time::ZERO {
                return Err(EditError::NegativeStart { clip: *member });
            }
        }

        // Lift every member out first, then place them all back. Checking
        // mid-move would compare a moved member against siblings that have not
        // moved yet and report a false overlap.
        let snapshot = self.clone();
        for member in &members {
            self.lift_clip(*member);
        }

        for member in &members {
            let Some((track_index, _)) = snapshot.locate_clip(*member) else {
                continue;
            };
            let Some(original) = snapshot.clip(*member) else {
                continue;
            };
            let mut clip = original.clip.clone();
            clip.timeline_start = clip.timeline_start.saturating_add(delta);

            if let Some(other) = self.find_overlap(track_index, &clip) {
                *self = snapshot;
                return Err(EditError::Overlap {
                    clip: *member,
                    other,
                });
            }
            self.insert_clip_sorted(track_index, clip);
        }
        Ok(())
    }

    /// Removes a clip from its track without touching groups.
    fn lift_clip(&mut self, clip_id: ClipId) -> Option<Clip> {
        let (track_index, position) = self.locate_clip(clip_id)?;
        Some(self.tracks[track_index].clips.remove(position))
    }

    fn insert_clip_sorted(&mut self, track_index: usize, clip: Clip) {
        let position = self.tracks[track_index]
            .clips
            .partition_point(|existing| existing.timeline_start <= clip.timeline_start);
        self.tracks[track_index].clips.insert(position, clip);
    }

    /// Trims one edge of a clip.
    ///
    /// `new_edge` is a timeline position; which edge it means is chosen by
    /// `edge`. Trimming the start also moves `timeline_start`, so the rest of
    /// the clip stays put on screen — this is "ripple-free" trimming, matching
    /// how an NLE behaves when you drag the in-point.
    pub fn trim_clip(
        &mut self,
        clip_id: ClipId,
        edge: TrimEdge,
        new_edge: Time,
        rate: FrameRate,
    ) -> Result<(), EditError> {
        let (track_index, position) = self
            .locate_clip(clip_id)
            .ok_or(EditError::NotFound(clip_id))?;

        let original = self.tracks[track_index].clips[position].clone();
        let source_duration = self
            .source(original.source_id)
            .map(|source| source.duration)
            .unwrap_or(original.source_out);
        let min = min_length(rate);

        let mut clip = original.clone();
        match edge {
            TrimEdge::Start => {
                let delta = new_edge.saturating_sub(original.timeline_start);
                let new_source_in = original.source_in.saturating_add(delta);

                if new_source_in < Time::ZERO {
                    return Err(EditError::BeforeSourceStart { clip: clip_id });
                }
                let new_len = clip.source_out.saturating_sub(new_source_in);
                if new_len < min {
                    return Err(EditError::TooShort { clip: clip_id, min });
                }
                if new_edge < Time::ZERO {
                    return Err(EditError::NegativeStart { clip: clip_id });
                }

                clip.source_in = new_source_in;
                clip.timeline_start = new_edge;
            }
            TrimEdge::End => {
                let delta = new_edge.saturating_sub(original.timeline_end());
                let new_source_out = original.source_out.saturating_add(delta);

                if new_source_out > source_duration {
                    return Err(EditError::PastSourceEnd {
                        clip: clip_id,
                        attempted: new_source_out,
                        source_duration,
                    });
                }
                let new_len = new_source_out.saturating_sub(clip.source_in);
                if new_len < min {
                    return Err(EditError::TooShort { clip: clip_id, min });
                }
                clip.source_out = new_source_out;
            }
        }

        // A trim may legitimately butt exactly against a neighbour, so the
        // overlap test uses the new range and the half-open interval rule.
        if let Some(other) = self.find_overlap_excluding(track_index, &clip, clip_id) {
            return Err(EditError::Overlap {
                clip: clip_id,
                other,
            });
        }

        self.tracks[track_index].clips[position] = clip;
        self.tracks[track_index]
            .clips
            .sort_by_key(|c| c.timeline_start);
        Ok(())
    }

    /// Removes a clip. Its group siblings survive but lose the grouping, since
    /// a group with a hole in it is confusing to move.
    pub fn delete_clip(&mut self, clip_id: ClipId) -> Result<(), EditError> {
        let (track_index, position) = self
            .locate_clip(clip_id)
            .ok_or(EditError::NotFound(clip_id))?;

        let group = self.tracks[track_index].clips[position].group;
        self.tracks[track_index].clips.remove(position);

        if let Some(group) = group {
            let remaining = self
                .clips()
                .filter(|entry| entry.clip.group == Some(group))
                .count();
            if remaining <= 1 {
                for track in &mut self.tracks {
                    for clip in &mut track.clips {
                        if clip.group == Some(group) {
                            clip.group = None;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Merges two clips that are adjacent on the timeline.
    ///
    /// Same source and contiguous source range become one clip; anything else
    /// becomes a group, which keeps both clips (and the visible seam) but makes
    /// them select and move together.
    pub fn merge_clips(
        &mut self,
        left_id: ClipId,
        right_id: ClipId,
    ) -> Result<MergeOutcome, EditError> {
        let left = self
            .clip(left_id)
            .map(|entry| entry.clip.clone())
            .ok_or(EditError::NotFound(left_id))?;
        let right = self
            .clip(right_id)
            .map(|entry| entry.clip.clone())
            .ok_or(EditError::NotFound(right_id))?;

        // Accept either argument order.
        let (left, right) = if left.timeline_start <= right.timeline_start {
            (left, right)
        } else {
            (right, left)
        };

        if left.timeline_end() != right.timeline_start {
            return Err(EditError::NotAdjacent {
                left: left.id,
                right: right.id,
            });
        }

        let truly_contiguous = left.source_id == right.source_id
            && left.source_out == right.source_in
            && left.group.is_none()
            && right.group.is_none();

        if truly_contiguous {
            let (track_index, position) = self
                .locate_clip(right.id)
                .ok_or(EditError::NotFound(right.id))?;
            self.tracks[track_index].clips.remove(position);

            let (left_track, left_pos) = self
                .locate_clip(left.id)
                .ok_or(EditError::NotFound(left.id))?;
            self.tracks[left_track].clips[left_pos].source_out = right.source_out;

            return Ok(MergeOutcome::Merged {
                kept: left.id,
                removed: right.id,
            });
        }

        // Cross-source or cut ranges: group them. Reuse an existing group id
        // when either side already belongs to one.
        let group = left
            .group
            .or(right.group)
            .unwrap_or_else(|| self.alloc_group_id());
        self.set_group(left.id, group);
        self.set_group(right.id, group);

        Ok(MergeOutcome::Grouped { group })
    }

    /// Clips whose group matches, in timeline order.
    pub fn group_members(&self, group: u64) -> Vec<ClipId> {
        self.clips()
            .filter(|entry| entry.clip.group == Some(group))
            .map(|entry| entry.clip.id)
            .collect()
    }

    /// Pair of clips on the same track that touch at `at`, if any.
    pub fn adjacent_pair_at(&self, clip_id: ClipId) -> Option<(ClipId, ClipId)> {
        let (track_index, position) = self.locate_clip(clip_id)?;
        let clips = &self.tracks[track_index].clips;
        let current = &clips[position];

        // Prefer the pair that starts at this clip; fall back to the one ending
        // here, so pressing merge on either side of a seam does the same thing.
        if let Some(next) = clips.get(position + 1) {
            if next.timeline_start == current.timeline_end() {
                return Some((current.id, next.id));
            }
        }
        if position > 0 {
            let previous = &clips[position - 1];
            if previous.timeline_end() == current.timeline_start {
                return Some((previous.id, current.id));
            }
        }
        None
    }

    // --- internals -------------------------------------------------------

    fn locate_clip(&self, clip_id: ClipId) -> Option<(usize, usize)> {
        for (track_index, track) in self.tracks.iter().enumerate() {
            if let Some(position) = track.clips.iter().position(|clip| clip.id == clip_id) {
                return Some((track_index, position));
            }
        }
        None
    }

    fn find_overlap(&self, track_index: usize, clip: &Clip) -> Option<ClipId> {
        self.find_overlap_excluding(track_index, clip, clip.id)
    }

    fn find_overlap_excluding(
        &self,
        track_index: usize,
        clip: &Clip,
        ignore: ClipId,
    ) -> Option<ClipId> {
        let range = clip.timeline_range();
        self.tracks[track_index]
            .clips
            .iter()
            .find(|other| other.id != ignore && other.timeline_range().intersect(range).is_some())
            .map(|other| other.id)
    }

    fn set_group(&mut self, clip_id: ClipId, group: u64) {
        if let Some((track_index, position)) = self.locate_clip(clip_id) {
            self.tracks[track_index].clips[position].group = Some(group);
        }
    }
}

/// Which edge of a clip a trim operates on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrimEdge {
    Start,
    End,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaSource, SourceId};
    use crate::media::{probe, KeyframeIndex};

    const FPS30: FrameRate = FrameRate { num: 30, den: 1 };

    fn source(id: u64, seconds: f64) -> MediaSource {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        let mut info = probe::parse(std::path::Path::new("a.mp4"), &json).unwrap();
        info.duration = Time::from_secs(seconds);
        MediaSource::new(
            SourceId(id),
            info,
            KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(seconds)]),
        )
    }

    fn project_with(seconds: &[f64]) -> Project {
        let mut project = Project::empty();
        for (index, len) in seconds.iter().enumerate() {
            let id = project.alloc_source_id();
            let mut src = source(0, *len);
            src.id = id;
            project.add_source(src);
            let _ = index;
        }
        project
    }

    fn first_clip(project: &Project) -> ClipId {
        project.video_track().unwrap().clips[0].id
    }

    fn clip(project: &Project, id: ClipId) -> Clip {
        project.clip(id).unwrap().clip.clone()
    }

    // --- split ---------------------------------------------------------

    #[test]
    fn split_produces_two_contiguous_clips() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);

        project.split_clip(id, Time::from_secs(4.0), FPS30).unwrap();

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, id, "left half keeps the original id");
        assert_eq!(clips[0].timeline_start, Time::ZERO);
        assert_eq!(clips[0].timeline_end(), Time::from_secs(4.0));
        assert_eq!(clips[1].timeline_start, Time::from_secs(4.0));
        assert_eq!(clips[1].timeline_end(), Time::from_secs(10.0));
        // Source ranges stay contiguous too.
        assert_eq!(clips[0].source_out, clips[1].source_in);
        // Total duration is unchanged.
        assert_eq!(project.duration(), Time::from_secs(10.0));
    }

    #[test]
    fn split_rejects_points_outside_the_clip() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        assert!(matches!(
            project.split_clip(id, Time::ZERO, FPS30),
            Err(EditError::SplitOutsideClip { .. })
        ));
        assert!(matches!(
            project.split_clip(id, Time::from_secs(10.0), FPS30),
            Err(EditError::SplitOutsideClip { .. })
        ));
        assert_eq!(project.video_track().unwrap().clips.len(), 1);
    }

    #[test]
    fn split_rejects_slivers() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        // 1 ms is far below one frame.
        assert!(matches!(
            project.split_clip(id, Time(1_000), FPS30),
            Err(EditError::TooShort { .. })
        ));
    }

    #[test]
    fn split_on_the_second_clip_keeps_order() {
        let mut project = project_with(&[4.0, 4.0]);
        let second = project.video_track().unwrap().clips[1].id;
        project
            .split_clip(second, Time::from_secs(6.0), FPS30)
            .unwrap();

        let starts: Vec<Time> = project
            .video_track()
            .unwrap()
            .clips
            .iter()
            .map(|c| c.timeline_start)
            .collect();
        assert_eq!(
            starts,
            vec![Time::ZERO, Time::from_secs(4.0), Time::from_secs(6.0)]
        );
    }

    // --- move ----------------------------------------------------------

    #[test]
    fn move_reorders_clips() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second_start = project.video_track().unwrap().clips[1].timeline_start;

        // Move the first clip to the far right, right after the second.
        project
            .move_clip(first, second_start.saturating_add(Time::from_secs(4.0)))
            .unwrap();

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips[0].timeline_start, Time::from_secs(4.0));
        assert_eq!(clips[1].id, first);
        assert_eq!(clips[1].timeline_start, Time::from_secs(8.0));
    }

    #[test]
    fn move_rejects_overlap() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;

        let err = project.move_clip(first, Time::from_secs(2.0)).unwrap_err();
        assert!(matches!(err, EditError::Overlap { .. }));
        // Nothing moved.
        assert_eq!(
            project.video_track().unwrap().clips[0].timeline_start,
            Time::ZERO
        );
    }

    #[test]
    fn move_allows_exact_abutment() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        // Land exactly where the second clip ends: no overlap.
        project.move_clip(first, Time::from_secs(8.0)).unwrap();
        assert_eq!(
            project.video_track().unwrap().clips[1].timeline_start,
            Time::from_secs(8.0)
        );
    }

    #[test]
    fn move_rejects_negative_start() {
        let mut project = project_with(&[4.0]);
        let id = first_clip(&project);
        assert!(matches!(
            project.move_clip(id, Time(-1_000_000)),
            Err(EditError::NegativeStart { .. })
        ));
    }

    // --- trim ----------------------------------------------------------

    #[test]
    fn trim_start_moves_in_point_and_keeps_out_point() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);

        project
            .trim_clip(id, TrimEdge::Start, Time::from_secs(2.0), FPS30)
            .unwrap();

        let c = clip(&project, id);
        assert_eq!(c.timeline_start, Time::from_secs(2.0));
        assert_eq!(c.source_in, Time::from_secs(2.0));
        assert_eq!(c.timeline_end(), Time::from_secs(10.0));
        assert_eq!(c.duration(), Time::from_secs(8.0));
    }

    #[test]
    fn trim_end_shortens_the_clip() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);

        project
            .trim_clip(id, TrimEdge::End, Time::from_secs(6.0), FPS30)
            .unwrap();

        let c = clip(&project, id);
        assert_eq!(c.timeline_end(), Time::from_secs(6.0));
        assert_eq!(c.source_out, Time::from_secs(6.0));
    }

    #[test]
    fn trim_end_cannot_exceed_the_source() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);

        let err = project
            .trim_clip(id, TrimEdge::End, Time::from_secs(12.0), FPS30)
            .unwrap_err();
        assert!(matches!(err, EditError::PastSourceEnd { .. }));
    }

    #[test]
    fn trim_start_cannot_go_before_source_start() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        // Trim the in-point earlier than the source allows.
        let err = project
            .trim_clip(id, TrimEdge::Start, Time(-1_000_000), FPS30)
            .unwrap_err();
        assert!(matches!(
            err,
            EditError::BeforeSourceStart { .. } | EditError::NegativeStart { .. }
        ));
    }

    #[test]
    fn trim_cannot_eat_into_the_neighbour() {
        // Build a clip whose source_in is non-zero, so extending its in-point
        // leftwards is legal against the source but collides with the previous
        // clip.
        let mut project = project_with(&[10.0]);
        let whole = first_clip(&project);
        project
            .split_clip(whole, Time::from_secs(5.0), FPS30)
            .unwrap();

        let right = project.video_track().unwrap().clips[1].id;
        assert_eq!(clip(&project, right).source_in, Time::from_secs(5.0));

        // Its source allows going back to 0, i.e. timeline 0, but the left
        // clip occupies 0-5 s.
        let err = project
            .trim_clip(right, TrimEdge::Start, Time::from_secs(2.0), FPS30)
            .unwrap_err();
        assert!(
            matches!(err, EditError::Overlap { .. }),
            "expected overlap, got {err:?}"
        );

        let c = clip(&project, right);
        assert_eq!(c.timeline_start, Time::from_secs(5.0));
        assert_eq!(c.source_in, Time::from_secs(5.0));
    }

    #[test]
    fn trim_start_leftward_works_when_there_is_room() {
        let mut project = project_with(&[10.0]);
        let whole = first_clip(&project);
        project
            .split_clip(whole, Time::from_secs(5.0), FPS30)
            .unwrap();
        let right = project.video_track().unwrap().clips[1].id;

        // Delete the left half so the space is free, then extend leftwards.
        project.delete_clip(whole).unwrap();
        project
            .trim_clip(right, TrimEdge::Start, Time::from_secs(2.0), FPS30)
            .unwrap();

        let c = clip(&project, right);
        assert_eq!(c.timeline_start, Time::from_secs(2.0));
        assert_eq!(c.source_in, Time::from_secs(2.0));
        assert_eq!(c.timeline_end(), Time::from_secs(10.0));
    }

    #[test]
    fn trim_cannot_go_before_its_source_origin() {
        let mut project = project_with(&[4.0, 4.0]);
        let second = project.video_track().unwrap().clips[1].id;

        // The second clip's source_in is already 0, so there is nothing to give.
        let err = project
            .trim_clip(second, TrimEdge::Start, Time::from_secs(2.0), FPS30)
            .unwrap_err();
        assert!(
            matches!(err, EditError::BeforeSourceStart { .. }),
            "expected source-start refusal, got {err:?}"
        );
    }

    #[test]
    fn trim_rejects_a_sub_frame_result() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        let err = project
            .trim_clip(id, TrimEdge::End, Time(10_000), FPS30)
            .unwrap_err();
        assert!(matches!(err, EditError::TooShort { .. }));
    }

    // --- delete --------------------------------------------------------

    #[test]
    fn delete_removes_the_clip_and_shortens_the_timeline() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;

        project.delete_clip(first).unwrap();

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].timeline_start, Time::from_secs(4.0));
        // The remaining clip keeps its absolute position (gaps are allowed).
        assert_eq!(project.duration(), Time::from_secs(8.0));
    }

    #[test]
    fn delete_unknown_clip_errors() {
        let mut project = project_with(&[4.0]);
        assert!(matches!(
            project.delete_clip(ClipId(999)),
            Err(EditError::NotFound(_))
        ));
    }

    // --- merge ---------------------------------------------------------

    #[test]
    fn merge_same_source_contiguous_truly_merges() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        project.split_clip(id, Time::from_secs(4.0), FPS30).unwrap();
        let right = project.video_track().unwrap().clips[1].id;

        let outcome = project.merge_clips(id, right).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                kept: id,
                removed: right
            }
        );

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].duration(), Time::from_secs(10.0));
    }

    #[test]
    fn merge_different_sources_groups_instead() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;

        let outcome = project.merge_clips(first, second).unwrap();
        let MergeOutcome::Grouped { group } = outcome else {
            panic!("expected a group, got {outcome:?}");
        };

        // Both clips survive.
        assert_eq!(project.video_track().unwrap().clips.len(), 2);
        assert_eq!(project.group_members(group).len(), 2);
    }

    #[test]
    fn merge_rejects_clips_that_do_not_touch() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;
        project.move_clip(second, Time::from_secs(20.0)).unwrap();

        assert!(matches!(
            project.merge_clips(first, second),
            Err(EditError::NotAdjacent { .. })
        ));
    }

    #[test]
    fn merge_is_order_independent() {
        let mut project = project_with(&[10.0]);
        let id = first_clip(&project);
        project.split_clip(id, Time::from_secs(4.0), FPS30).unwrap();
        let right = project.video_track().unwrap().clips[1].id;

        // Pass them reversed.
        let outcome = project.merge_clips(right, id).unwrap();
        assert!(matches!(outcome, MergeOutcome::Merged { .. }));
    }

    #[test]
    fn grouped_clips_move_together() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;
        project.merge_clips(first, second).unwrap();

        // Move the group right by 2 s.
        project.move_group(first, Time::from_secs(2.0)).unwrap();

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips[0].timeline_start, Time::from_secs(2.0));
        assert_eq!(clips[1].timeline_start, Time::from_secs(6.0));
    }

    #[test]
    fn moving_a_group_that_cannot_fit_changes_nothing() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;
        project.merge_clips(first, second).unwrap();

        // Move the group to start at -1 s: both members would go negative.
        assert!(project.move_group(first, Time(-1_000_000)).is_err());

        let clips = &project.video_track().unwrap().clips;
        assert_eq!(clips[0].timeline_start, Time::ZERO);
        assert_eq!(clips[1].timeline_start, Time::from_secs(4.0));
    }

    #[test]
    fn deleting_a_group_member_dissolves_the_group() {
        let mut project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;
        project.merge_clips(first, second).unwrap();

        project.delete_clip(first).unwrap();

        let remaining = &project.video_track().unwrap().clips[0];
        assert_eq!(remaining.group, None);
    }

    #[test]
    fn adjacent_pair_lookup_works_from_either_side() {
        let project = project_with(&[4.0, 4.0]);
        let first = project.video_track().unwrap().clips[0].id;
        let second = project.video_track().unwrap().clips[1].id;

        assert_eq!(project.adjacent_pair_at(first), Some((first, second)));
        assert_eq!(project.adjacent_pair_at(second), Some((first, second)));
    }

    #[test]
    fn adjacent_pair_is_none_when_there_is_a_gap() {
        let mut project = project_with(&[4.0, 4.0]);
        let second = project.video_track().unwrap().clips[1].id;
        project.move_clip(second, Time::from_secs(20.0)).unwrap();
        let first = project.video_track().unwrap().clips[0].id;
        assert_eq!(project.adjacent_pair_at(first), None);
        let _ = second;
    }
}
