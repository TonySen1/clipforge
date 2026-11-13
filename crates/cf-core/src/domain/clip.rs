//! Clips and tracks.
//!
//! A clip is a window into a source: `[source_in, source_out)` placed at
//! `timeline_start`. The invariant every function here preserves is that clips
//! on one track never overlap and are ordered by `timeline_start`.

use crate::domain::SourceId;
use crate::time::{Time, TimeRange};

/// Stable clip identifier, unique within one project.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClipId(pub u64);

/// Stable track identifier.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(pub u64);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Video => "V",
            Self::Audio => "A",
        }
    }
}

/// One placed span of a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clip {
    pub id: ClipId,
    pub source_id: SourceId,
    /// In-point inside the source.
    pub source_in: Time,
    /// Out-point inside the source, exclusive.
    pub source_out: Time,
    /// Where the clip starts on the timeline.
    pub timeline_start: Time,
    /// Clips sharing a group move and select together (cross-source merges).
    pub group: Option<u64>,
    /// Display label captured at import time; falls back to the file name.
    pub name: String,
}

impl Clip {
    pub fn duration(&self) -> Time {
        self.source_out.saturating_sub(self.source_in)
    }

    pub fn timeline_end(&self) -> Time {
        self.timeline_start.saturating_add(self.duration())
    }

    pub fn timeline_range(&self) -> TimeRange {
        TimeRange {
            start: self.timeline_start,
            end: self.timeline_end(),
        }
    }

    /// Maps a timeline position to a position inside the source.
    pub fn source_time_at(&self, timeline: Time) -> Time {
        let offset = timeline.saturating_sub(self.timeline_start);
        self.source_in.saturating_add(offset).min(self.source_out)
    }

    /// Maps a source position back to the timeline, if it lands inside.
    pub fn timeline_time_of_source(&self, source: Time, rate: crate::FrameRate) -> Option<Time> {
        let _ = rate;
        if source < self.source_in || source >= self.source_out {
            return None;
        }
        Some(
            self.timeline_start
                .saturating_add(source.saturating_sub(self.source_in)),
        )
    }

    pub fn contains_timeline(&self, t: Time) -> bool {
        self.timeline_start <= t && t < self.timeline_end()
    }
}

/// An ordered, non-overlapping lane of clips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    /// Number shown in the track head ("V1", "A1").
    pub index: u32,
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn new(id: TrackId, kind: TrackKind, index: u32) -> Self {
        Self {
            id,
            kind,
            index,
            clips: Vec::new(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}{}", self.kind.short_label(), self.index)
    }

    /// Inserts a clip, keeping the track sorted. Returns `false` when the clip
    /// would overlap an existing one; the track is left untouched.
    pub fn insert(&mut self, clip: Clip) -> bool {
        if self.overlaps(&clip) {
            return false;
        }
        let position = self
            .clips
            .partition_point(|existing| existing.timeline_start <= clip.timeline_start);
        self.clips.insert(position, clip);
        true
    }

    fn overlaps(&self, clip: &Clip) -> bool {
        let range = clip.timeline_range();
        self.clips.iter().any(|existing| {
            existing.id != clip.id && existing.timeline_range().intersect(range).is_some()
        })
    }

    /// The clip covering `t`, if any.
    pub fn clip_at(&self, t: Time) -> Option<&Clip> {
        self.clips.iter().find(|clip| clip.contains_timeline(t))
    }

    pub fn end(&self) -> Time {
        self.clips
            .last()
            .map(Clip::timeline_end)
            .unwrap_or(Time::ZERO)
    }

    /// Clips intersecting `range`, in timeline order.
    pub fn clips_in(&self, range: TimeRange) -> impl Iterator<Item = &Clip> {
        self.clips
            .iter()
            .filter(move |clip| clip.timeline_range().intersect(range).is_some())
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.iter().find(|clip| clip.id == id)
    }

    /// Gap immediately before `t`, for snapping and "no clip here" checks.
    pub fn is_gap_at(&self, t: Time) -> bool {
        self.clip_at(t).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u64, start: f64, dur: f64) -> Clip {
        Clip {
            id: ClipId(id),
            source_id: SourceId(1),
            source_in: Time::ZERO,
            source_out: Time::from_secs(dur),
            timeline_start: Time::from_secs(start),
            group: None,
            name: format!("clip{id}"),
        }
    }

    fn track() -> Track {
        Track::new(TrackId(1), TrackKind::Video, 1)
    }

    #[test]
    fn clip_maps_timeline_to_source() {
        let c = Clip {
            id: ClipId(1),
            source_id: SourceId(1),
            source_in: Time::from_secs(10.0),
            source_out: Time::from_secs(20.0),
            timeline_start: Time::from_secs(5.0),
            group: None,
            name: "c".into(),
        };
        assert_eq!(c.duration(), Time::from_secs(10.0));
        assert_eq!(c.timeline_end(), Time::from_secs(15.0));
        // 5 s into the timeline is 10 s into the source.
        assert_eq!(
            c.source_time_at(Time::from_secs(5.0)),
            Time::from_secs(10.0)
        );
        assert_eq!(
            c.source_time_at(Time::from_secs(7.0)),
            Time::from_secs(12.0)
        );
        // Past the end, it clamps instead of running away.
        assert_eq!(
            c.source_time_at(Time::from_secs(99.0)),
            Time::from_secs(20.0)
        );
    }

    #[test]
    fn clip_contains_uses_half_open_range() {
        let c = clip(1, 5.0, 10.0);
        assert!(!c.contains_timeline(Time::from_secs(4.9)));
        assert!(c.contains_timeline(Time::from_secs(5.0)));
        assert!(c.contains_timeline(Time::from_secs(14.9)));
        assert!(!c.contains_timeline(Time::from_secs(15.0)));
    }

    #[test]
    fn insert_keeps_order() {
        let mut t = track();
        assert!(t.insert(clip(2, 10.0, 5.0)));
        assert!(t.insert(clip(1, 0.0, 5.0)));
        assert!(t.insert(clip(3, 20.0, 5.0)));
        let starts: Vec<_> = t.clips.iter().map(|c| c.timeline_start).collect();
        assert_eq!(
            starts,
            vec![Time::ZERO, Time::from_secs(10.0), Time::from_secs(20.0)]
        );
    }

    #[test]
    fn insert_rejects_overlap() {
        let mut t = track();
        assert!(t.insert(clip(1, 0.0, 10.0)));
        assert!(!t.insert(clip(2, 5.0, 10.0)));
        assert!(!t.insert(clip(3, 9.9, 1.0)));
        assert_eq!(t.clips.len(), 1);
    }

    #[test]
    fn insert_allows_touching_boundaries() {
        let mut t = track();
        assert!(t.insert(clip(1, 0.0, 10.0)));
        // Starts exactly where the previous one ends: not an overlap.
        assert!(t.insert(clip(2, 10.0, 10.0)));
        assert_eq!(t.clips.len(), 2);
    }

    #[test]
    fn clip_at_finds_the_covering_clip() {
        let mut t = track();
        t.insert(clip(1, 0.0, 10.0));
        t.insert(clip(2, 20.0, 10.0));

        assert_eq!(
            t.clip_at(Time::from_secs(5.0)).map(|c| c.id),
            Some(ClipId(1))
        );
        assert_eq!(t.clip_at(Time::from_secs(15.0)), None);
        assert!(t.is_gap_at(Time::from_secs(15.0)));
        assert_eq!(
            t.clip_at(Time::from_secs(20.0)).map(|c| c.id),
            Some(ClipId(2))
        );
    }

    #[test]
    fn end_uses_the_last_clip() {
        let mut t = track();
        assert_eq!(t.end(), Time::ZERO);
        t.insert(clip(1, 0.0, 10.0));
        t.insert(clip(2, 20.0, 5.0));
        assert_eq!(t.end(), Time::from_secs(25.0));
    }

    #[test]
    fn clips_in_returns_intersecting_clips() {
        let mut t = track();
        t.insert(clip(1, 0.0, 10.0));
        t.insert(clip(2, 20.0, 10.0));
        t.insert(clip(3, 40.0, 10.0));

        let range = TimeRange::new(Time::from_secs(5.0), Time::from_secs(25.0)).unwrap();
        let ids: Vec<_> = t.clips_in(range).map(|c| c.id).collect();
        assert_eq!(ids, vec![ClipId(1), ClipId(2)]);
    }

    #[test]
    fn track_label_matches_kind() {
        assert_eq!(track().label(), "V1");
        let audio = Track::new(TrackId(2), TrackKind::Audio, 1);
        assert_eq!(audio.label(), "A1");
    }
}
