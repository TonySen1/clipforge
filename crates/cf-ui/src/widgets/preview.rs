//! Video preview stage.
//!
//! Two states share one layout: an empty placeholder and a decoded frame.
//! The frame is letterboxed inside the stage rect so switching between sources
//! of different aspect ratios never shifts the surrounding chrome.

use crate::theme::{Metrics, Palette};
use egui::{
    pos2, vec2, Align2, Color32, FontId, Rect, Response, Sense, Shape, Stroke, StrokeKind,
    TextureHandle, Ui,
};

/// Draws the empty state: no source selected, or nothing decoded yet.
pub fn preview_empty(ui: &mut Ui, p: &Palette, empty_title: &str, empty_hint: &str) -> Response {
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::hover());
    let painter = ui.painter();

    painter.rect(
        rect,
        Metrics::RADIUS_LG_C,
        p.bg_stage,
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );

    let stage = fit_16_9(rect);
    painter.rect(
        stage,
        Metrics::RADIUS_MD_C,
        p.bg_stage_inset,
        Stroke::new(1.0, p.border_strong.gamma_multiply(0.6)),
        StrokeKind::Inside,
    );

    // The well is black in dark mode and light grey in light mode, so the
    // empty state uses the `on_stage_*` pair, which flips with it.
    let icon_center = pos2(stage.center().x, stage.center().y - 18.0);
    draw_empty_badge(painter, p, icon_center, 62.0);

    painter.text(
        pos2(stage.center().x, stage.center().y + 44.0),
        Align2::CENTER_CENTER,
        empty_title,
        FontId::proportional(15.0),
        p.on_stage_primary,
    );
    painter.text(
        pos2(stage.center().x, stage.center().y + 68.0),
        Align2::CENTER_CENTER,
        empty_hint,
        FontId::proportional(12.0),
        p.on_stage_muted,
    );

    response
}

/// Draws a decoded frame, letterboxed to preserve its aspect ratio.
///
/// `frame_aspect` is width/height of the *decoded* frame, which already has
/// rotation baked in.
pub fn preview_frame(
    ui: &mut Ui,
    p: &Palette,
    texture: &TextureHandle,
    frame_aspect: f32,
) -> Response {
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::hover());
    let painter = ui.painter();

    painter.rect(
        rect,
        Metrics::RADIUS_LG_C,
        p.bg_stage,
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );

    let stage = fit_16_9(rect);
    let image = fit_aspect(stage, frame_aspect);

    // Once a frame is showing there is no drawn frame at all: square corners,
    // no outline. Rounding clipped the picture's corners, and any border made
    // the image look boxed in rather than simply present. The inset fill stays
    // so pillarbox/letterbox bars still have a deliberate colour.
    painter.rect_filled(stage, 0.0, p.bg_stage_inset);
    painter.add(egui::Shape::image(
        texture.id(),
        image,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    ));

    response
}

/// A centered 16:9 box inside `outer`, used for the stage in both states.
fn fit_16_9(outer: Rect) -> Rect {
    let avail = outer.shrink(18.0);
    let (mut w, mut h) = (avail.width(), avail.width() * 9.0 / 16.0);
    if h > avail.height() {
        h = avail.height();
        w = h * 16.0 / 9.0;
    }
    Rect::from_center_size(outer.center(), vec2(w, h))
}

/// Largest rect with `aspect` centered inside `outer`.
fn fit_aspect(outer: Rect, aspect: f32) -> Rect {
    if !aspect.is_finite() || aspect <= 0.0 {
        return outer;
    }
    let avail = outer.shrink(2.0);
    let (mut w, mut h) = (avail.width(), avail.width() / aspect);
    if h > avail.height() {
        h = avail.height();
        w = h * aspect;
    }
    Rect::from_center_size(outer.center(), vec2(w, h))
}

fn draw_empty_badge(painter: &egui::Painter, p: &Palette, center: egui::Pos2, size: f32) {
    painter.circle_filled(center, size * 0.5, p.accent_soft);
    painter.circle_stroke(
        center,
        size * 0.5,
        Stroke::new(1.0, p.accent.gamma_multiply(0.45)),
    );

    // Play triangle, optically centered.
    let r = size * 0.17;
    let tri = vec![
        pos2(center.x - r * 0.72, center.y - r),
        pos2(center.x - r * 0.72, center.y + r),
        pos2(center.x + r, center.y),
    ];
    painter.add(Shape::convex_polygon(tri, p.accent, Stroke::NONE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_nine_stage_is_centered() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 1000.0));
        let stage = fit_16_9(outer);
        assert!((stage.width() / stage.height() - 16.0 / 9.0).abs() < 0.01);
        assert_eq!(stage.center(), outer.center());
    }

    #[test]
    fn wide_stage_shrinks_to_height() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), vec2(2000.0, 300.0));
        let stage = fit_16_9(outer);
        assert!(stage.height() <= 300.0);
        assert!(stage.width() <= 2000.0);
    }

    #[test]
    fn portrait_frame_fits_by_height() {
        let stage = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 562.0));
        let image = fit_aspect(stage, 9.0 / 16.0);
        assert!(image.height() <= stage.height());
        assert!((image.width() / image.height() - 9.0 / 16.0).abs() < 0.01);
    }

    #[test]
    fn landscape_frame_fits_by_width() {
        let stage = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 562.0));
        let image = fit_aspect(stage, 21.0 / 9.0);
        assert!(image.width() <= stage.width());
        assert!((image.width() / image.height() - 21.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn invalid_aspect_falls_back_to_full_rect() {
        let stage = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        assert_eq!(fit_aspect(stage, 0.0), stage);
        assert_eq!(fit_aspect(stage, f32::NAN), stage);
    }
}
