//! Snapping targets for timeline dragging.
//!
//! Snapping is a pure function of (candidate time, candidates, pixel
//! threshold, scale) so the behaviour is testable and the drawing code can
//! reuse the same result to decide which guide line to paint.

use crate::widgets::timeline_view::TimelineView;
use cf_core::domain::Project;
use cf_core::time::Time;
use cf_core::{ClipId, FrameRate};

/// What the pointer snapped onto, for drawing a guide and a hint label.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SnapKind {
    /// Time zero.
    Origin,
    /// The playhead.
    Playhead,
    /// A clip edge.
    ClipEdge,
    /// A whole frame boundary.
    Frame,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SnapResult {
    pub time: Time,
    pub kind: Option<SnapKind>,
    /// Pixel distance the pointer was pulled, for debugging and tests.
    pub delta_px: f32,
}

impl SnapResult {
    pub fn unsnapped(time: Time) -> Self {
        Self {
            time,
            kind: None,
            delta_px: 0.0,
        }
    }

    pub fn is_snapped(&self) -> bool {
        self.kind.is_some()
    }
}

/// Distance in pixels within which a target wins.
pub const SNAP_PX: f32 = 8.0;

/// Builds the snap candidate list for a drag on one clip.
///
/// The dragged clip's own edges are excluded: snapping an edge to itself would
/// pin the clip in place and make dragging feel broken.
pub fn candidates(
    project: &Project,
    exclude: Option<ClipId>,
    playhead: Time,
) -> Vec<(Time, SnapKind)> {
    let mut out = vec![
        (Time::ZERO, SnapKind::Origin),
        (playhead, SnapKind::Playhead),
    ];

    for entry in project.clips() {
        if Some(entry.clip.id) == exclude {
            continue;
        }
        out.push((entry.clip.timeline_start, SnapKind::ClipEdge));
        out.push((entry.clip.timeline_end(), SnapKind::ClipEdge));
    }

    out
}

/// Snaps `time` to the nearest candidate within `threshold_px`.
///
/// Multiple edges of the dragged clip are considered at once (`edges`), so
/// dragging a clip snaps when *either* of its ends lines up — this is what
/// makes butt-joining clips feel natural.
pub fn snap(
    time: Time,
    edges: &[Time],
    candidates: &[(Time, SnapKind)],
    view: &TimelineView,
    threshold_px: f32,
) -> SnapResult {
    let mut best: Option<SnapResult> = None;

    for (candidate, kind) in candidates {
        for edge in edges {
            // Distance from this edge of the dragged clip to the candidate.
            let offset = candidate.saturating_sub(*edge);
            let px = (offset.as_secs_f64() * view.px_per_sec as f64).abs() as f32;
            if px > threshold_px {
                continue;
            }

            let snapped_time = time.saturating_add(offset);
            if snapped_time < Time::ZERO {
                continue;
            }

            let better = match &best {
                None => true,
                Some(current) => px < current.delta_px,
            };
            if better {
                best = Some(SnapResult {
                    time: snapped_time,
                    kind: Some(*kind),
                    delta_px: px,
                });
            }
        }
    }

    best.unwrap_or_else(|| SnapResult::unsnapped(time))
}

/// Snaps to the nearest whole frame, used when no other target is near.
pub fn snap_to_frame(
    time: Time,
    rate: FrameRate,
    view: &TimelineView,
    threshold_px: f32,
) -> SnapResult {
    if rate.num == 0 || rate.den == 0 {
        return SnapResult::unsnapped(time);
    }

    let frame = rate.frame_at(time);
    let snapped = rate.time_of_frame(frame).max(Time::ZERO);
    let px = ((snapped.micros() - time.micros()).unsigned_abs() as f64 / 1_000_000.0
        * view.px_per_sec as f64) as f32;

    if px <= threshold_px {
        SnapResult {
            time: snapped,
            kind: Some(SnapKind::Frame),
            delta_px: px,
        }
    } else {
        SnapResult::unsnapped(time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_core::domain::{MediaSource, SourceId};
    use cf_core::media::{probe, KeyframeIndex};

    fn view() -> TimelineView {
        // 100 px per second: 8 px == 80 ms.
        TimelineView::new(100.0, 1000.0)
    }

    fn project(seconds: &[f64]) -> Project {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();

        let mut project = Project::empty();
        for len in seconds {
            let id = project.alloc_source_id();
            let mut info = probe::parse(std::path::Path::new("a.mp4"), &json).unwrap();
            info.duration = Time::from_secs(*len);
            let mut source = MediaSource::new(
                SourceId(0),
                info,
                KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(*len)]),
            );
            source.id = id;
            project.add_source(source);
        }
        project
    }

    #[test]
    fn snaps_to_a_nearby_clip_edge() {
        let project = project(&[4.0, 4.0]);
        let candidates = candidates(&project, None, Time::ZERO);

        // 4.05 s is 5 px from the 4 s edge at the default 0.15 s window.
        let result = snap(
            Time::from_secs(4.05),
            &[Time::from_secs(4.05)],
            &candidates,
            &view(),
            SNAP_PX,
        );

        assert!(result.is_snapped());
        assert_eq!(result.time, Time::from_secs(4.0));
        assert!(result.delta_px <= SNAP_PX);
    }

    #[test]
    fn ignores_targets_further_than_the_threshold() {
        let project = project(&[4.0, 4.0]);
        let candidates = candidates(&project, None, Time::ZERO);

        // 4.2 s is 20 px from the 4 s edge: outside the 8 px window.
        let result = snap(
            Time::from_secs(4.2),
            &[Time::from_secs(4.2)],
            &candidates,
            &view(),
            SNAP_PX,
        );

        assert!(!result.is_snapped());
        assert_eq!(result.time, Time::from_secs(4.2));
    }

    #[test]
    fn picks_the_closest_of_several_targets() {
        let candidates = vec![
            (Time::from_secs(1.0), SnapKind::ClipEdge),
            (Time::from_secs(1.05), SnapKind::Playhead),
        ];

        let result = snap(
            Time::from_secs(1.04),
            &[Time::from_secs(1.04)],
            &candidates,
            &view(),
            SNAP_PX,
        );

        assert_eq!(result.time, Time::from_secs(1.05));
        assert_eq!(result.kind, Some(SnapKind::Playhead));
    }

    #[test]
    fn excludes_the_dragged_clip() {
        // A gap between the two clips makes each edge distinguishable.
        let mut project = project(&[4.0, 4.0]);
        let second = project.video_track().unwrap().clips[1].id;
        project.move_clip(second, Time::from_secs(10.0)).unwrap();

        let first = project.video_track().unwrap().clips[0].id;
        let candidates = candidates(&project, Some(first), Time::ZERO);

        // Origin is always a target. The dragged clip's own 4 s edge is not.
        assert!(candidates
            .iter()
            .any(|(t, kind)| *t == Time::ZERO && *kind == SnapKind::Origin));
        assert!(!candidates.iter().any(|(t, _)| *t == Time::from_secs(4.0)));
        // ...but the other clip's edges (10 s and 14 s) still are.
        assert!(candidates.iter().any(|(t, _)| *t == Time::from_secs(10.0)));
        assert!(candidates.iter().any(|(t, _)| *t == Time::from_secs(14.0)));
    }

    #[test]
    fn snaps_either_edge_of_the_dragged_clip() {
        let candidates = vec![(Time::from_secs(10.0), SnapKind::ClipEdge)];

        // Dragging a 2 s clip by its trailing edge: start 8.04, end 10.04.
        let result = snap(
            Time::from_secs(8.04),
            &[Time::from_secs(8.04), Time::from_secs(10.04)],
            &candidates,
            &view(),
            SNAP_PX,
        );

        // The end edge is 4 px from 10 s, so the whole clip shifts to 7.96.
        assert!(result.is_snapped());
        assert_eq!(result.time, Time::from_secs(8.0));
    }

    #[test]
    fn never_snaps_below_zero() {
        let candidates = vec![(Time::ZERO, SnapKind::Origin)];
        // Dragging so far left that snapping to 0 would go negative.
        let result = snap(
            Time(-1_000_000),
            &[Time(-1_000_000)],
            &candidates,
            &view(),
            SNAP_PX,
        );
        assert!(!result.is_snapped());
        assert_eq!(result.time, Time(-1_000_000));
    }

    #[test]
    fn frame_snapping_rounds_to_the_grid() {
        let rate = FrameRate { num: 30, den: 1 };
        let result = snap_to_frame(Time(35_000), rate, &view(), SNAP_PX);

        assert!(result.is_snapped());
        assert_eq!(result.kind, Some(SnapKind::Frame));
        assert_eq!(result.time, Time(33_333));
    }

    #[test]
    fn frame_snapping_is_skipped_when_the_grid_is_dense() {
        // Zoomed way in, one frame is hundreds of pixels: always snap.
        let zoomed = TimelineView::new(600.0, 1000.0);
        let rate = FrameRate { num: 30, den: 1 };
        let result = snap_to_frame(Time(33_000), rate, &zoomed, SNAP_PX);
        assert!(result.is_snapped());

        // Zoomed far out, a frame is a fraction of a pixel: never snap.
        let wide = TimelineView::new(2.0, 1000.0);
        let result = snap_to_frame(Time(16_000), rate, &wide, SNAP_PX);
        assert!(
            result.is_snapped(),
            "16 ms is 0.03 px away, well inside the threshold"
        );
    }

    #[test]
    fn degenerate_frame_rate_does_not_snap() {
        let rate = FrameRate { num: 0, den: 0 };
        let result = snap_to_frame(Time::from_secs(1.0), rate, &view(), SNAP_PX);
        assert!(!result.is_snapped());
    }

    #[test]
    fn playhead_is_always_a_candidate() {
        let project = project(&[4.0]);
        let candidates = candidates(&project, None, Time::from_secs(2.5));
        assert!(candidates
            .iter()
            .any(|(t, kind)| *t == Time::from_secs(2.5) && *kind == SnapKind::Playhead));
    }
}
