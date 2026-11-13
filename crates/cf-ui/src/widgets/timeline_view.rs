//! Timeline geometry: the mapping between time and pixels.
//!
//! Kept separate from drawing so the arithmetic (zoom anchoring, scroll
//! clamping, tick selection) can be unit tested without an egui context.
//! Everything here is pure: `TimelineView` is a value, not a widget.

use cf_core::time::{Time, TimeRange};

/// Pixel distance from the left edge of the track area to time zero.
pub const TRACK_HEAD_WIDTH: f32 = 64.0;

/// Minimum and maximum horizontal zoom, in pixels per second.
pub const MIN_PX_PER_SEC: f32 = 2.0;
pub const MAX_PX_PER_SEC: f32 = 600.0;
pub const DEFAULT_PX_PER_SEC: f32 = 48.0;

/// Which part of the timeline is visible and at what scale.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TimelineView {
    /// Timeline time at the left edge of the track area.
    pub scroll: Time,
    /// Horizontal scale.
    pub px_per_sec: f32,
    /// Width of the track area in pixels.
    pub width: f32,
}

impl Default for TimelineView {
    fn default() -> Self {
        Self {
            scroll: Time::ZERO,
            px_per_sec: DEFAULT_PX_PER_SEC,
            width: 800.0,
        }
    }
}

impl TimelineView {
    pub fn new(px_per_sec: f32, width: f32) -> Self {
        Self {
            scroll: Time::ZERO,
            px_per_sec: px_per_sec.clamp(MIN_PX_PER_SEC, MAX_PX_PER_SEC),
            width: width.max(1.0),
        }
    }

    /// Duration covered by the visible area.
    pub fn visible_duration(&self) -> Time {
        Time::from_secs((self.width / self.px_per_sec) as f64)
    }

    pub fn visible_range(&self) -> TimeRange {
        TimeRange {
            start: self.scroll,
            end: self.scroll.saturating_add(self.visible_duration()),
        }
    }

    /// Timeline time -> x offset **relative to the track area's left edge**.
    pub fn time_to_x(&self, t: Time) -> f32 {
        let seconds = t.saturating_sub(self.scroll).as_secs_f64();
        (seconds * self.px_per_sec as f64) as f32
    }

    /// x offset relative to the track area's left edge -> timeline time.
    pub fn x_to_time(&self, x: f32) -> Time {
        let seconds = (x / self.px_per_sec) as f64;
        self.scroll
            .saturating_add(Time::from_secs(seconds))
            .max(Time::ZERO)
    }

    /// Clamps scrolling so the view never runs past `content_end`.
    ///
    /// At maximum scroll the end of the content sits at the right edge, which
    /// is the conventional NLE behaviour; content shorter than the window
    /// cannot scroll at all.
    pub fn clamp_scroll(&mut self, content_end: Time) {
        let visible = self.visible_duration();
        let max_scroll = if content_end > visible {
            content_end.saturating_sub(visible)
        } else {
            Time::ZERO
        };
        self.scroll = self.scroll.clamp(Time::ZERO, max_scroll);
    }

    pub fn scroll_by(&mut self, delta: Time, content_end: Time) {
        self.scroll = self.scroll.saturating_add(delta).max(Time::ZERO);
        self.clamp_scroll(content_end);
    }

    /// Zooms around `anchor_x` so the time under the cursor stays put.
    pub fn zoom_at(&mut self, anchor_x: f32, factor: f32, content_end: Time) {
        let anchor_time = self.x_to_time(anchor_x);
        self.px_per_sec = (self.px_per_sec * factor).clamp(MIN_PX_PER_SEC, MAX_PX_PER_SEC);

        // Put `anchor_time` back under `anchor_x` at the new scale.
        let offset = (anchor_x / self.px_per_sec) as f64;
        self.scroll = anchor_time.saturating_sub(Time::from_secs(offset));
        self.clamp_scroll(content_end);
    }

    /// Fits `content_end` into the visible width, with a little padding.
    pub fn fit(&mut self, content_end: Time) {
        let seconds = content_end.as_secs_f64().max(1.0);
        self.scroll = Time::ZERO;
        self.px_per_sec = ((self.width * 0.96) as f64 / seconds) as f32;
        self.px_per_sec = self.px_per_sec.clamp(MIN_PX_PER_SEC, MAX_PX_PER_SEC);
    }
}

/// A tick mark on the ruler.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Tick {
    pub time: Time,
    /// Relative to the track area's left edge.
    pub x: f32,
    /// Major ticks get a label and a taller mark.
    pub major: bool,
}

/// Chooses an adaptive tick step so labels never collide.
///
/// Returns `(major_step, minor_step)` in seconds. Minor ticks subdivide each
/// major interval into 4 or 5 parts, whichever the step divides cleanly.
pub fn tick_steps(px_per_sec: f32) -> (f64, f64) {
    // Aim for a labelled tick roughly every 90 px.
    let target = 90.0 / px_per_sec.max(0.001) as f64;

    const STEPS: [f64; 16] = [
        0.04, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0,
        1800.0,
    ];

    let major = STEPS
        .into_iter()
        .find(|step| *step >= target)
        .unwrap_or(3600.0);

    let minor = if major >= 60.0 {
        // Minutes and up: 4 subdivisions reads better than 5 at this length.
        major / 4.0
    } else if (major / 5.0 * 100.0).round() / 100.0 > 0.0 {
        major / 5.0
    } else {
        major / 4.0
    };

    (major, minor)
}

/// Builds the ticks visible in `view`, capped to avoid pathological loops.
pub fn visible_ticks(view: &TimelineView, max_ticks: usize) -> Vec<Tick> {
    let (major_step, minor_step) = tick_steps(view.px_per_sec);
    let minor_px = minor_step * view.px_per_sec as f64;
    // Below ~4 px apart, minor ticks are visual noise.
    let include_minor = minor_px >= 4.0;

    let step = if include_minor {
        minor_step
    } else {
        major_step
    };
    let step_us = (step * 1_000_000.0).round() as i64;
    if step_us <= 0 {
        return Vec::new();
    }

    let start = view.scroll.micros().div_euclid(step_us) * step_us;
    let end = view.visible_range().end.micros();

    let major_us = (major_step * 1_000_000.0).round() as i64;
    let mut ticks = Vec::new();
    let mut t = start;
    while t <= end && ticks.len() < max_ticks {
        // The first multiple of `step_us` can sit before the view's left edge;
        // those ticks would paint outside the track area, so skip them.
        if t >= 0 && t >= view.scroll.micros() {
            let time = Time(t);
            ticks.push(Tick {
                time,
                x: view.time_to_x(time),
                major: major_us > 0 && t % major_us == 0,
            });
        }
        t += step_us;
    }
    ticks
}

/// Formats a ruler label, dropping fields that are always zero.
pub fn format_tick(time: Time, major_step: f64) -> String {
    let total_ms = time.micros().max(0) / 1_000;
    let secs_total = total_ms / 1_000;
    let h = secs_total / 3_600;
    let m = (secs_total / 60) % 60;
    let s = secs_total % 60;
    let frames_ms = total_ms % 1_000;

    if major_step < 1.0 {
        // Sub-second ticks show milliseconds so neighbouring labels differ.
        if h > 0 {
            format!("{h}:{m:02}:{s:02}.{frames_ms:03}")
        } else {
            format!("{m}:{s:02}.{frames_ms:03}")
        }
    } else if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> TimelineView {
        TimelineView::new(50.0, 1000.0)
    }

    #[test]
    fn round_trips_time_and_x() {
        let view = view();
        for seconds in [0.0, 1.0, 7.5, 20.0] {
            let t = Time::from_secs(seconds);
            let x = view.time_to_x(t);
            let back = view.x_to_time(x);
            assert!(
                (back.micros() - t.micros()).abs() <= 1_000,
                "round trip failed for {t}: got {back}"
            );
        }
    }

    #[test]
    fn x_to_time_accounts_for_scroll() {
        let mut view = view();
        view.scroll = Time::from_secs(10.0);
        assert_eq!(view.x_to_time(0.0), Time::from_secs(10.0));
        assert_eq!(view.x_to_time(50.0), Time::from_secs(11.0));
    }

    #[test]
    fn x_to_time_never_goes_negative() {
        let mut view = view();
        view.scroll = Time::ZERO;
        assert_eq!(view.x_to_time(-100.0), Time::ZERO);
    }

    #[test]
    fn visible_duration_follows_scale() {
        let view = view();
        assert_eq!(view.visible_duration(), Time::from_secs(20.0));
    }

    #[test]
    fn zoom_keeps_the_anchor_time_in_place() {
        let mut view = view();
        view.px_per_sec = 20.0;
        view.width = 1000.0;
        let anchor_x = 500.0;
        let before = view.x_to_time(anchor_x);

        view.zoom_at(anchor_x, 2.0, Time::from_secs(600.0));
        let after = view.x_to_time(anchor_x);

        assert!(
            (after.micros() - before.micros()).abs() <= 2_000,
            "anchor drifted from {before} to {after}"
        );
        assert_eq!(view.px_per_sec, 40.0);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut view = view();
        view.zoom_at(0.0, 1000.0, Time::from_secs(60.0));
        assert_eq!(view.px_per_sec, MAX_PX_PER_SEC);

        view.zoom_at(0.0, 0.00001, Time::from_secs(60.0));
        assert_eq!(view.px_per_sec, MIN_PX_PER_SEC);
    }

    #[test]
    fn scroll_is_clamped_to_content() {
        let mut view = view();
        view.scroll_by(Time::from_secs(1000.0), Time::from_secs(30.0));
        // 30 s of content in a 20 s window leaves 10 s of scroll, minus the pad.
        assert!(view.scroll <= Time::from_secs(10.0));
        assert!(view.scroll >= Time::ZERO);
    }

    #[test]
    fn scrolling_back_stops_at_zero() {
        let mut view = view();
        view.scroll_by(Time::from_secs(-50.0), Time::from_secs(30.0));
        assert_eq!(view.scroll, Time::ZERO);
    }

    #[test]
    fn fit_fills_the_width() {
        let mut view = view();
        view.fit(Time::from_secs(100.0));
        assert_eq!(view.scroll, Time::ZERO);
        assert!((view.px_per_sec - 9.6).abs() < 0.01);
    }

    #[test]
    fn fit_handles_zero_length_content() {
        let mut view = view();
        view.fit(Time::ZERO);
        assert!(view.px_per_sec >= MIN_PX_PER_SEC);
        assert!(view.px_per_sec.is_finite());
    }

    #[test]
    fn tick_step_grows_as_you_zoom_out() {
        let dense = tick_steps(500.0).0;
        let sparse = tick_steps(2.0).0;
        assert!(dense < sparse, "dense={dense} sparse={sparse}");
    }

    #[test]
    fn ticks_stay_within_the_visible_range() {
        let mut view = view();
        view.scroll = Time::from_secs(5.0);
        let ticks = visible_ticks(&view, 10_000);
        assert!(!ticks.is_empty());
        for tick in &ticks {
            assert!(tick.time >= Time::ZERO);
            assert!(
                tick.x >= -1.0 && tick.x <= view.width + 1.0,
                "tick at {}",
                tick.x
            );
        }
    }

    #[test]
    fn ticks_are_capped() {
        let mut view = TimelineView::new(MAX_PX_PER_SEC, 4000.0);
        view.scroll = Time::ZERO;
        let ticks = visible_ticks(&view, 100);
        assert!(ticks.len() <= 100);
    }

    #[test]
    fn major_ticks_are_a_subset_of_all_ticks() {
        let ticks = visible_ticks(&view(), 10_000);
        assert!(ticks.iter().any(|t| t.major));
        // Every major tick lands on a round multiple of the major step.
        let (major_step, _) = tick_steps(view().px_per_sec);
        let major_us = (major_step * 1_000_000.0) as i64;
        for tick in ticks.iter().filter(|t| t.major) {
            assert_eq!(tick.time.micros() % major_us, 0);
        }
    }

    #[test]
    fn tick_labels_drop_leading_zero_fields() {
        assert_eq!(format_tick(Time::from_secs(65.0), 1.0), "1:05");
        assert_eq!(format_tick(Time::from_secs(3_725.0), 1.0), "1:02:05");
        assert_eq!(format_tick(Time(500_000), 0.1), "0:00.500");
    }
}
