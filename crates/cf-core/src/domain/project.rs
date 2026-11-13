//! The editable document.
//!
//! `Project` holds only what can be undone: sources, tracks, playhead. Theme
//! and locale live in `Preferences` because they are application settings, not
//! document state.

use crate::domain::clip::{Clip, ClipId, Track, TrackId, TrackKind};
use crate::domain::{MediaSource, SourceId};
use crate::time::{Time, TimeRange};

/// A clip plus the track it lives on; what the UI needs to draw one row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClipRef<'a> {
    pub track: &'a Track,
    pub clip: &'a Clip,
}

/// Not serializable yet: `MediaSource` owns caches that are rebuilt from disk
/// on load. Project files arrive in P7 as a separate on-disk format.
#[derive(Clone, Debug, Default)]
pub struct Project {
    pub sources: Vec<MediaSource>,
    pub tracks: Vec<Track>,
    pub playhead: Time,
    next_clip_id: u64,
    next_track_id: u64,
    next_source_id: u64,
    next_group_id: u64,
}

// `MediaSource` holds `Arc<MediaInfo>` and `Arc<KeyframeIndex>`, neither of
// which is serializable by design. The project is not persisted yet (P7), so
// these impls only exist to keep the derive above honest about intent.
impl PartialEq for Project {
    fn eq(&self, other: &Self) -> bool {
        self.tracks == other.tracks
            && self.playhead == other.playhead
            && self.sources.len() == other.sources.len()
    }
}

impl Project {
    /// Creates an empty project with one video and one audio track.
    ///
    /// The audio track mirrors the video track in P3: it is rendered but not
    /// independently editable, which keeps the MVP free of A/V sync bugs.
    pub fn empty() -> Self {
        let mut project = Self {
            sources: Vec::new(),
            tracks: Vec::new(),
            playhead: Time::ZERO,
            next_clip_id: 1,
            next_track_id: 1,
            next_source_id: 1,
            next_group_id: 1,
        };

        let video = project.alloc_track(TrackKind::Video, 1);
        let audio = project.alloc_track(TrackKind::Audio, 1);
        project.tracks.push(video);
        project.tracks.push(audio);
        project
    }

    fn alloc_track(&mut self, kind: TrackKind, index: u32) -> Track {
        let id = TrackId(self.next_track_id);
        self.next_track_id += 1;
        Track::new(id, kind, index)
    }

    pub fn alloc_clip_id(&mut self) -> ClipId {
        let id = ClipId(self.next_clip_id);
        self.next_clip_id += 1;
        id
    }

    /// Group ids are never reused within a session; they only need to be
    /// unique among live groups.
    pub fn alloc_group_id(&mut self) -> u64 {
        let id = self.next_group_id;
        self.next_group_id += 1;
        id
    }

    pub fn alloc_source_id(&mut self) -> SourceId {
        let id = SourceId(self.next_source_id);
        self.next_source_id += 1;
        id
    }

    /// Registers an imported source and appends a clip for it at the end of
    /// the video track.
    pub fn add_source(&mut self, source: MediaSource) -> ClipId {
        let source_id = source.id;
        let name = source.display_name();
        let duration = source.duration;
        self.sources.push(source);

        let clip_id = ClipId(self.next_clip_id);
        self.next_clip_id += 1;
        let clip = Clip {
            id: clip_id,
            source_id,
            source_in: Time::ZERO,
            source_out: duration,
            timeline_start: self.video_track().map(Track::end).unwrap_or(Time::ZERO),
            group: None,
            name,
        };

        if let Some(track) = self.video_track_mut() {
            track.insert(clip);
        }

        clip_id
    }

    pub fn video_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn video_track_mut(&mut self) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.kind == TrackKind::Video)
    }

    pub fn audio_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Audio)
    }

    /// Total editable length: the end of the last clip on the video track.
    pub fn duration(&self) -> Time {
        self.video_track().map(Track::end).unwrap_or(Time::ZERO)
    }

    pub fn source(&self, id: SourceId) -> Option<&MediaSource> {
        self.sources.iter().find(|s| s.id == id)
    }

    /// Resolves a timeline position to the source and time that should be
    /// shown there. Used by the preview to seek.
    pub fn resolve(&self, timeline: Time) -> Option<(&MediaSource, Time)> {
        let track = self.video_track()?;
        let clip = track.clip_at(timeline)?;
        let source = self.source(clip.source_id)?;
        Some((source, clip.source_time_at(timeline)))
    }

    /// Every clip on every track, in draw order (video first).
    pub fn clips(&self) -> impl Iterator<Item = ClipRef<'_>> {
        self.tracks
            .iter()
            .flat_map(|track| track.clips.iter().map(move |clip| ClipRef { track, clip }))
    }

    pub fn clip(&self, id: ClipId) -> Option<ClipRef<'_>> {
        self.clips().find(|entry| entry.clip.id == id)
    }

    /// Clips intersecting a timeline range, across all tracks.
    pub fn clips_in(&self, range: TimeRange) -> Vec<ClipRef<'_>> {
        self.clips()
            .filter(|entry| entry.clip.timeline_range().intersect(range).is_some())
            .collect()
    }

    /// Total duration of the media placed on the video track (excludes gaps).
    pub fn placed_duration(&self) -> Time {
        let mut total = Time::ZERO;
        if let Some(track) = self.video_track() {
            for clip in &track.clips {
                total = total.saturating_add(clip.duration());
            }
        }
        total
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Removes every empty span by sliding clips left until they touch.
    ///
    /// Returns a copy so callers can plan an export without disturbing the
    /// user's timeline: the gaps are a view/export concern, not an edit.
    /// Clips keep their source in/out points, so nothing is trimmed.
    pub fn without_gaps(&self) -> Self {
        let mut out = self.clone();
        for track in &mut out.tracks {
            let mut cursor = Time::ZERO;
            for clip in &mut track.clips {
                clip.timeline_start = cursor;
                cursor = clip.timeline_end();
            }
        }
        out
    }

    pub fn set_playhead(&mut self, t: Time) {
        self.playhead = t.clamp(Time::ZERO, self.duration());
    }

    /// Moves the playhead by `delta`, clamped to the timeline.
    pub fn nudge_playhead(&mut self, delta: Time) {
        self.set_playhead(self.playhead.saturating_add(delta));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{probe, KeyframeIndex};

    fn sample(name: &str, seconds: f64) -> MediaSource {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        let mut info = probe::parse(std::path::Path::new(name), &json).unwrap();
        info.duration = Time::from_secs(seconds);
        MediaSource::new(
            SourceId(0),
            info,
            KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(seconds)]),
        )
    }

    fn project_with(durations: &[f64]) -> Project {
        let mut project = Project::empty();
        for (index, seconds) in durations.iter().enumerate() {
            let id = project.alloc_source_id();
            let mut source = sample(&format!("clip{index}.mp4"), *seconds);
            source.id = id;
            project.add_source(source);
        }
        project
    }

    #[test]
    fn empty_project_has_two_tracks_and_no_duration() {
        let project = Project::empty();
        assert_eq!(project.tracks.len(), 2);
        assert_eq!(project.video_track().unwrap().label(), "V1");
        assert_eq!(project.audio_track().unwrap().label(), "A1");
        assert_eq!(project.duration(), Time::ZERO);
        assert!(project.is_empty());
    }

    #[test]
    fn sources_are_appended_end_to_end() {
        let project = project_with(&[3.0, 5.0, 2.0]);
        let track = project.video_track().unwrap();
        assert_eq!(track.clips.len(), 3);

        let starts: Vec<Time> = track.clips.iter().map(|c| c.timeline_start).collect();
        assert_eq!(
            starts,
            vec![Time::ZERO, Time::from_secs(3.0), Time::from_secs(8.0)]
        );
        assert_eq!(project.duration(), Time::from_secs(10.0));
        assert_eq!(project.placed_duration(), Time::from_secs(10.0));
    }

    #[test]
    fn resolves_timeline_positions_across_clips() {
        let project = project_with(&[3.0, 5.0]);

        let (source, t) = project.resolve(Time::from_secs(2.0)).unwrap();
        assert!(source.path().to_string_lossy().contains("clip0"));
        assert_eq!(t, Time::from_secs(2.0));

        // 7 s on the timeline is 4 s into the second clip (which starts at 3 s).
        let (source, t) = project.resolve(Time::from_secs(7.0)).unwrap();
        assert!(source.path().to_string_lossy().contains("clip1"));
        assert_eq!(t, Time::from_secs(4.0));

        // Past the end there is nothing to resolve.
        assert!(project.resolve(Time::from_secs(20.0)).is_none());
    }

    #[test]
    fn without_gaps_butts_clips_together() {
        let mut project = project_with(&[3.0, 2.0]);
        let second = project.video_track().unwrap().clips[1].id;
        // Leave a 4 s hole between the two clips.
        project.move_clip(second, Time::from_secs(7.0)).unwrap();
        assert_eq!(project.duration(), Time::from_secs(9.0));

        let packed = project.without_gaps();
        let starts: Vec<Time> = packed
            .video_track()
            .unwrap()
            .clips
            .iter()
            .map(|c| c.timeline_start)
            .collect();
        assert_eq!(starts, vec![Time::ZERO, Time::from_secs(3.0)]);
        assert_eq!(packed.duration(), Time::from_secs(5.0));

        // Source ranges are untouched: this is a layout change, not a trim.
        let clip = &packed.video_track().unwrap().clips[1];
        assert_eq!(clip.source_in, Time::ZERO);
        assert_eq!(clip.source_out, Time::from_secs(2.0));

        // The original project still has its gap.
        assert_eq!(project.duration(), Time::from_secs(9.0));
    }

    #[test]
    fn without_gaps_keeps_a_contiguous_timeline_unchanged() {
        let project = project_with(&[4.0, 4.0]);
        assert_eq!(project.without_gaps(), project);
    }

    #[test]
    fn playhead_is_clamped_to_the_timeline() {
        let mut project = project_with(&[4.0]);
        project.set_playhead(Time::from_secs(10.0));
        assert_eq!(project.playhead, Time::from_secs(4.0));

        project.set_playhead(Time::from_secs(-5.0));
        assert_eq!(project.playhead, Time::ZERO);

        project.nudge_playhead(Time::from_secs(1.0));
        assert_eq!(project.playhead, Time::from_secs(1.0));
    }

    #[test]
    fn clips_in_range_crosses_tracks() {
        let project = project_with(&[3.0, 5.0]);
        let range = TimeRange::new(Time::from_secs(2.0), Time::from_secs(4.0)).unwrap();
        let found = project.clips_in(range);
        // The 0-3 s clip ends inside the range and the 3-8 s clip starts at
        // its edge, so both intersect a 2-4 s window.
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .all(|entry| entry.track.kind == TrackKind::Video));
    }

    #[test]
    fn clip_lookup_by_id() {
        let project = project_with(&[3.0, 5.0]);
        let first = project.video_track().unwrap().clips[0].id;
        assert_eq!(
            project.clip(first).unwrap().clip.duration(),
            Time::from_secs(3.0)
        );
        assert!(project.clip(ClipId(9999)).is_none());
    }
}
