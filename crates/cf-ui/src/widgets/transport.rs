//! Playback transport bar.
//!
//! Icons are painted geometrically so the bar never depends on emoji or
//! symbol fonts being present.

use crate::theme::{Metrics, Palette};
use cf_core::time::Time;
use egui::{pos2, vec2, Rect, Sense, Shape, Stroke, Ui};

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TransportAction {
    PlayPause,
    PrevFrame,
    NextFrame,
    MuteToggle,
    /// Volume changed in 0.0..=1.0.
    Volume(f32),
}

/// Which editing controls are currently usable.
///
/// Passed in rather than derived so widgets stay dumb views: the app owns the
/// undo stack and the selection.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EditAvailability {
    pub has_selection: bool,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Glyph {
    PrevFrame,
    NextFrame,
    Play,
    Pause,
    Volume,
    Muted,
}

/// Draws the transport bar and returns the action the user triggered, if any.
///
/// Editing controls live here rather than in the toolbar: they act on the
/// timeline, which sits right below, and the transport is where the eye already
/// is while scrubbing.
#[allow(clippy::too_many_arguments)]
pub fn transport(
    ui: &mut Ui,
    p: &Palette,
    position: Time,
    duration: Time,
    playing: bool,
    muted: bool,
    volume: f32,
    enabled: bool,
    icons: Option<&crate::icons::IconSet>,
) -> Option<TransportAction> {
    let mut action = None;
    // The bar sits on the panel, not on the stage, so it uses panel text
    // tokens; only the "not loaded yet" state falls back to muted.
    let active = p.text_primary;
    let idle = p.text_muted;
    // Absolute layout: the cluster is placed at the exact horizontal center of
    // the whole row, and the side controls hug the edges. A column layout was
    // tried first and drifted whenever the timecode text width changed.
    // Take the full remaining height (the caller reserves exactly
    // TRANSPORT_HEIGHT for this row) and center the content inside it, so the
    // bar never looks top-heavy no matter how tall the slot is.
    let row = ui.available_rect_before_wrap();
    let height = row.height().clamp(20.0, Metrics::TRANSPORT_HEIGHT);
    let bar = Rect::from_min_size(row.min, vec2(row.width(), height));

    let response = ui.allocate_rect(bar, Sense::hover());
    let painter = ui.painter();

    // --- timecode, left aligned ---------------------------------------
    let time_color = if enabled { active } else { idle };
    let position_text = painter.layout_no_wrap(
        format_timecode(position),
        egui::FontId::monospace(13.0),
        time_color,
    );
    let slash_text = painter.layout_no_wrap("/".to_owned(), egui::FontId::proportional(12.0), idle);
    let duration_text = painter.layout_no_wrap(
        format_timecode(duration),
        egui::FontId::monospace(13.0),
        idle,
    );

    let mut x = bar.left() + 2.0;
    let y = bar.center().y - position_text.size().y * 0.5;
    painter.galley(pos2(x, y), position_text.clone(), time_color);
    x += position_text.size().x + 6.0;
    painter.galley(
        pos2(x, bar.center().y - slash_text.size().y * 0.5),
        slash_text.clone(),
        idle,
    );
    x += slash_text.size().x + 6.0;
    painter.galley(
        pos2(x, bar.center().y - duration_text.size().y * 0.5),
        duration_text.clone(),
        idle,
    );

    // --- transport cluster, centered -----------------------------------
    const BUTTON_W: f32 = 28.0;
    const BUTTON_GAP: f32 = 2.0;
    const BUTTON_H: f32 = 24.0;
    let cluster_width = BUTTON_W * 3.0 + BUTTON_GAP * 2.0;
    let cluster_left = bar.center().x - cluster_width * 0.5;

    let buttons = [
        (Glyph::PrevFrame, TransportAction::PrevFrame),
        (
            if playing { Glyph::Pause } else { Glyph::Play },
            TransportAction::PlayPause,
        ),
        (Glyph::NextFrame, TransportAction::NextFrame),
    ];

    for (index, (glyph, kind)) in buttons.into_iter().enumerate() {
        let rect = Rect::from_min_size(
            pos2(
                cluster_left + index as f32 * (BUTTON_W + BUTTON_GAP),
                bar.center().y - BUTTON_H * 0.5,
            ),
            vec2(BUTTON_W, BUTTON_H),
        );
        let hit = ui.interact(
            rect,
            ui.scope_id().with(("transport", index)),
            Sense::click(),
        );
        let hit = hit.on_hover_cursor(egui::CursorIcon::PointingHand);

        if enabled && (hit.hovered() || hit.is_pointer_button_down_on()) {
            let fill = if hit.is_pointer_button_down_on() {
                p.accent_soft
            } else {
                p.bg_hover
            };
            painter.rect_filled(rect, Metrics::RADIUS_SM_C, fill);
        }

        let color = if !enabled {
            p.text_muted
        } else {
            p.text_primary
        };
        draw_glyph(painter, rect, glyph, color);

        if enabled && hit.clicked() {
            action = Some(kind);
        }
    }

    // --- volume slider + mute, right aligned ---------------------------
    let slider_width = 84.0;
    let mute_rect = Rect::from_min_size(
        pos2(
            bar.right() - BUTTON_W - 2.0,
            bar.center().y - BUTTON_H * 0.5,
        ),
        vec2(BUTTON_W, BUTTON_H),
    );

    let slider_rect = Rect::from_min_size(
        pos2(mute_rect.left() - slider_width - 10.0, bar.center().y - 3.0),
        vec2(slider_width, 6.0),
    );
    let slider = ui.interact(
        slider_rect.expand2(vec2(0.0, 7.0)),
        ui.scope_id().with("transport_volume"),
        Sense::click_and_drag(),
    );

    // Track and filled portion.
    painter.rect_filled(slider_rect, Metrics::RADIUS_XS_C, p.bg_sunken);
    let fill_w = slider_rect.width() * volume.clamp(0.0, 1.0);
    if fill_w > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(slider_rect.min, vec2(fill_w, slider_rect.height())),
            Metrics::RADIUS_XS_C,
            p.accent,
        );
    }
    let knob_x = slider_rect.left() + fill_w;
    painter.circle_filled(
        pos2(knob_x, slider_rect.center().y),
        4.0,
        if slider.hovered() || slider.dragged() {
            p.accent_hover
        } else {
            p.accent
        },
    );

    if slider.dragged() || slider.clicked() {
        if let Some(pos) = slider.interact_pointer_pos() {
            let ratio = ((pos.x - slider_rect.left()) / slider_rect.width()).clamp(0.0, 1.0);
            action = Some(TransportAction::Volume(ratio));
        }
    }
    let mute_hit = ui
        .interact(
            mute_rect,
            ui.scope_id().with("transport_mute"),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    if mute_hit.hovered() || mute_hit.is_pointer_button_down_on() {
        let fill = if mute_hit.is_pointer_button_down_on() {
            p.accent_soft
        } else {
            p.bg_hover
        };
        painter.rect_filled(mute_rect, Metrics::RADIUS_SM_C, fill);
    }
    // The volume glyph comes from the icon set (a proper speaker outline);
    // the geometric fallback only exists for the first frames before textures
    // are resident.
    let icon = if muted {
        crate::icons::Icon::VolumeMuted
    } else {
        crate::icons::Icon::Volume
    };
    let mut drawn = false;
    if let Some(icons) = icons {
        if let Some(texture) = icons.get(icon) {
            // 22 px keeps the speaker's 1.6 px strokes above the point at
            // which linear downscaling smears them into noise.
            let side = 22.0;
            let image_rect = Rect::from_center_size(mute_rect.center(), vec2(side, side));
            let mut mesh = egui::Mesh::with_texture(texture.id());
            mesh.add_rect_with_uv(
                image_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                p.text_primary,
            );
            painter.add(egui::Shape::mesh(mesh));
            drawn = true;
        }
    }
    if !drawn {
        draw_glyph(
            painter,
            mute_rect,
            if muted { Glyph::Muted } else { Glyph::Volume },
            p.text_primary,
        );
    }
    if mute_hit.clicked() {
        action = Some(TransportAction::MuteToggle);
    }

    let _ = response;
    action
}

fn format_timecode(t: Time) -> String {
    let micros = t.micros().max(0);
    let total_ms = micros / 1_000;
    let ms = total_ms % 1_000;
    let total_secs = total_ms / 1_000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3_600;
    format!("{hours:02}:{mins:02}:{secs:02}.{ms:03}")
}

fn draw_glyph(painter: &egui::Painter, rect: egui::Rect, glyph: Glyph, color: egui::Color32) {
    let c = rect.center();
    let s = 9.0;
    match glyph {
        Glyph::Play => {
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(c.x - s * 0.55, c.y - s),
                    pos2(c.x - s * 0.55, c.y + s),
                    pos2(c.x + s * 0.85, c.y),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Pause => {
            let bar = vec2(3.0, s * 1.8);
            for dx in [-4.0, 4.0] {
                painter.rect_filled(
                    egui::Rect::from_center_size(pos2(c.x + dx, c.y), bar),
                    1.0,
                    color,
                );
            }
        }
        Glyph::PrevFrame | Glyph::NextFrame => {
            let dir = if glyph == Glyph::PrevFrame { -1.0 } else { 1.0 };
            // Triangle pointing in the step direction.
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(c.x - dir * s * 0.9, c.y - s),
                    pos2(c.x - dir * s * 0.9, c.y + s),
                    pos2(c.x + dir * s * 0.55, c.y),
                ],
                color,
                Stroke::NONE,
            ));
            // Stop bar on the outer edge.
            let bx = c.x + dir * s * 0.95;
            painter.line_segment(
                [pos2(bx, c.y - s), pos2(bx, c.y + s)],
                Stroke::new(2.0, color),
            );
        }
        Glyph::Volume | Glyph::Muted => {
            // Speaker = small rectangle (driver) + trapezoid cone, the same
            // construction most icon sets use. The cone's wide edge faces the
            // driver and its narrow edge faces right, so the silhouette reads
            // as a speaker rather than a pointed shape.
            let u = s * 0.9;

            // Driver block on the left.
            let driver = Rect::from_min_max(
                pos2(c.x - u * 1.00, c.y - u * 0.28),
                pos2(c.x - u * 0.45, c.y + u * 0.28),
            );
            painter.rect_filled(driver, 1.0, color);

            // Cone: wide at the driver, narrow at the far edge.
            let cone = vec![
                pos2(c.x - u * 0.50, c.y - u * 0.30),
                pos2(c.x + u * 0.10, c.y - u * 0.78),
                pos2(c.x + u * 0.10, c.y + u * 0.78),
                pos2(c.x - u * 0.50, c.y + u * 0.30),
            ];
            painter.add(Shape::convex_polygon(cone, color, Stroke::NONE));

            if glyph == Glyph::Muted {
                let cx = c.x + u * 0.62;
                let d = u * 0.30;
                let stroke = Stroke::new(1.6, color);
                painter.line_segment([pos2(cx - d, c.y - d), pos2(cx + d, c.y + d)], stroke);
                painter.line_segment([pos2(cx + d, c.y - d), pos2(cx - d, c.y + d)], stroke);
            } else {
                // Two arcs opening to the right, sampled from a circle so they
                // curve instead of forming a "greater-than" sign.
                let center = pos2(c.x + u * 0.10, c.y);
                for (radius, alpha, width) in [(0.55, 0.95, 1.5), (0.90, 0.6, 1.3)] {
                    let r = u * radius;
                    let stroke = Stroke::new(width, color.gamma_multiply(alpha));
                    let steps = 8;
                    for step in 0..steps {
                        let a0 = -0.75 + 1.5 * (step as f32 / steps as f32);
                        let a1 = -0.75 + 1.5 * ((step + 1) as f32 / steps as f32);
                        painter.line_segment(
                            [
                                pos2(center.x + r * a0.cos(), center.y + r * a0.sin()),
                                pos2(center.x + r * a1.cos(), center.y + r * a1.sin()),
                            ],
                            stroke,
                        );
                    }
                }
            }
        }
    }
}
