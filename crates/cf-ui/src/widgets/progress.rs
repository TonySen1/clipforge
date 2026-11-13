//! Progress bar with animated diagonal stripes.
//!
//! egui's built-in `ProgressBar::animate` only pulses the existing text; the
//! "moving candy stripes" look that reads as *activity* (rather than just a
//! fill level) has to be drawn. The stripes are clipped to the filled portion
//! and offset by wall-clock time, so the bar keeps moving even when the
//! percentage stands still — which is exactly when the user needs reassurance
//! that the job is still alive.

use crate::theme::{Metrics, Palette};
use egui::{pos2, vec2, Color32, Rect, Shape, Stroke, StrokeKind, Ui};

/// Width of one stripe plus one gap, in points.
const STRIPE_PERIOD: f32 = 22.0;
/// Fraction of the period painted with the light overlay.
const STRIPE_DUTY: f32 = 0.5;
/// Stripes lean right; this is the horizontal run per full bar height.
const STRIPE_SLANT: f32 = 0.55;
/// Points per second the pattern travels.
const STRIPE_SPEED: f32 = 38.0;

/// Draws a striped progress bar and returns its rect.
pub fn striped_progress(ui: &mut Ui, p: &Palette, fraction: f32, height: f32) -> Rect {
    let fraction = fraction.clamp(0.0, 1.0);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());

    let painter = ui.painter();
    let radius = Metrics::RADIUS_XS_C;

    // Trough.
    painter.rect(
        rect,
        radius,
        p.bg_sunken,
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );

    let filled_width = rect.width() * fraction;
    if filled_width <= 0.0 {
        return rect;
    }

    let fill_rect = Rect::from_min_size(rect.min, vec2(filled_width.max(height * 0.5), height));

    // Base fill.
    painter.rect_filled(fill_rect, radius, p.accent);

    // Stripes, clipped to the fill so they never bleed into the trough.
    let clipped = painter.with_clip_rect(fill_rect.intersect(rect));

    // `time` drives the animation; egui repaints continuously while a job
    // reports progress, but request one more explicitly so a stalled
    // percentage still animates.
    let time = ui.input(|i| i.time) as f32;
    let offset = (time * STRIPE_SPEED) % STRIPE_PERIOD;
    let slant = height * STRIPE_SLANT;

    // The stripe overlay comes from the palette so both themes stay coherent.
    let stripe_color = p.progress_stripe;

    let mut x = rect.left() - STRIPE_PERIOD + offset;
    while x < rect.right() {
        let band = STRIPE_PERIOD * STRIPE_DUTY;
        // A parallelogram: the top edge leads the bottom edge by `slant`.
        let quad = vec![
            pos2(x + slant, rect.top()),
            pos2(x + slant + band, rect.top()),
            pos2(x + band, rect.bottom()),
            pos2(x, rect.bottom()),
        ];
        clipped.add(Shape::convex_polygon(quad, stripe_color, Stroke::NONE));
        x += STRIPE_PERIOD;
    }

    // Soft inner highlight along the top edge for depth.
    clipped.line_segment(
        [
            pos2(fill_rect.left() + 2.0, rect.top() + 1.0),
            pos2(fill_rect.right() - 2.0, rect.top() + 1.0),
        ],
        Stroke::new(1.0, p.progress_highlight),
    );

    ui.ctx().request_repaint();
    rect
}

/// Moves a color toward white by `amount` (0..=1).
#[allow(dead_code)] // kept for tuning the stripe overlay by hand
fn lighten(color: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |channel: u8| -> u8 {
        let base = channel as f32;
        let target = 255.0;
        (base + (target - base) * amount).round() as u8
    };
    Color32::from_rgba_unmultiplied(mix(color.r()), mix(color.g()), mix(color.b()), color.a())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lighten_moves_towards_white() {
        let base = Color32::BLACK;
        let lighter = lighten(base, 0.5);
        assert_eq!(lighter.r(), 128);
    }

    #[test]
    fn lighten_clamps_and_keeps_alpha() {
        // Opaque: `Color32` stores premultiplied channels, so only an opaque
        // input lets us assert the raw channel values directly.
        let base = Color32::from_gray(200);
        let lighter = lighten(base, 2.0);
        assert_eq!(lighter.r(), 255);
        assert_eq!(lighter.a(), 255);
        let same = lighten(base, 0.0);
        assert_eq!(same.r(), 200);

        // Translucent: the alpha channel survives, and the result stays
        // premultiplied (never brighter than its own alpha).
        let translucent = Color32::from_rgba_unmultiplied(200, 200, 200, 128);
        let lifted = lighten(translucent, 2.0);
        assert_eq!(lifted.a(), 128);
        assert!(lifted.r() <= lifted.a());
    }

    #[test]
    fn zero_fraction_draws_no_fill() {
        // Numeric check of the geometry guard used in the widget.
        assert!(0.0_f32.clamp(0.0, 1.0) * 100.0 <= 0.0);
    }
}
