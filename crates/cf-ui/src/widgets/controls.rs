//! Small button helpers that keep call sites free of styling details.

use crate::theme::{Metrics, Palette};
use egui::{Button, Color32, Response, RichText, Stroke, Ui};

/// Filled, accent-colored button for the primary action in a view.
pub fn primary_button(ui: &mut Ui, p: &Palette, label: impl Into<String>) -> Response {
    let label = label.into();
    ui.scope(|ui| {
        let style = ui.style_mut();
        let w = &mut style.visuals.widgets;
        w.inactive.weak_bg_fill = p.accent;
        w.inactive.bg_fill = p.accent;
        w.inactive.bg_stroke = Stroke::new(1.0, p.accent);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text_on_accent);
        w.hovered.weak_bg_fill = p.accent_hover;
        w.hovered.bg_fill = p.accent_hover;
        w.hovered.bg_stroke = Stroke::new(1.0, p.accent_hover);
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_on_accent);
        w.active.weak_bg_fill = p.accent;
        w.active.bg_fill = p.accent;
        w.active.bg_stroke = Stroke::new(1.0, p.accent_hover);
        w.active.fg_stroke = Stroke::new(1.5, p.text_on_accent);

        ui.add(
            Button::new(RichText::new(label).color(p.text_on_accent).strong())
                .frame(true)
                .min_size(egui::vec2(0.0, Metrics::CONTROL_HEIGHT)),
        )
    })
    .inner
}

/// Outlined button for secondary actions.
pub fn subtle_button(ui: &mut Ui, p: &Palette, label: impl Into<String>) -> Response {
    let label = label.into();
    ui.scope(|ui| {
        let style = ui.style_mut();
        let w = &mut style.visuals.widgets;
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        w.inactive.bg_fill = Color32::TRANSPARENT;
        w.inactive.bg_stroke = Stroke::new(1.0, p.border);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text_secondary);
        w.hovered.weak_bg_fill = p.bg_hover;
        w.hovered.bg_fill = p.bg_hover;
        w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.0, p.accent);

        ui.add(
            Button::new(RichText::new(label).color(p.text_secondary))
                .frame(true)
                .min_size(egui::vec2(0.0, Metrics::CONTROL_HEIGHT)),
        )
    })
    .inner
}

/// Borderless button used in the toolbar.
pub fn ghost_button(ui: &mut Ui, p: &Palette, label: impl Into<String>) -> Response {
    let label = label.into();
    ui.scope(|ui| {
        let style = ui.style_mut();
        let w = &mut style.visuals.widgets;
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, p.text_secondary);
        w.hovered.weak_bg_fill = p.bg_hover;
        w.hovered.bg_stroke = Stroke::NONE;
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::NONE;
        w.active.fg_stroke = Stroke::new(1.0, p.accent);

        ui.add(
            Button::new(RichText::new(label).color(p.text_secondary))
                .frame(true)
                .min_size(egui::vec2(0.0, Metrics::CONTROL_HEIGHT)),
        )
    })
    .inner
}

/// Icon-only button for the toolbar.
///
/// `selected` gives the accent treatment used by the theme picker; `enabled`
/// greys the label out without hiding it, so the layout never shifts.
pub fn icon_button(
    ui: &mut Ui,
    p: &Palette,
    icons: &crate::icons::IconSet,
    icon: crate::icons::Icon,
    tooltip: &str,
    selected: bool,
    enabled: bool,
) -> Response {
    let size = egui::vec2(32.0, Metrics::CONTROL_HEIGHT);
    // A disabled button must not report clicks, so it only senses hover.
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let response = response.on_hover_text(tooltip);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();

    if selected || hovered || pressed {
        let fill = if selected || pressed {
            p.accent_soft
        } else {
            p.bg_hover
        };
        ui.painter().rect_filled(rect, Metrics::RADIUS_SM_C, fill);
    }

    let color = if !enabled {
        p.text_muted
    } else if selected {
        p.accent
    } else if hovered {
        p.text_primary
    } else {
        p.text_secondary
    };

    if let Some(texture) = icons.get(icon) {
        let side = 19.0;
        let image_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(side, side));
        let mut mesh = egui::Mesh::with_texture(texture.id());
        mesh.add_rect_with_uv(
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            color,
        );
        ui.painter().add(egui::Shape::mesh(mesh));
    }

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    response
}

/// A small pill/chip with a muted background, used for metadata.
pub fn chip(ui: &mut Ui, p: &Palette, text: impl Into<String>) {
    let text = text.into();
    egui::Frame::new()
        .fill(p.bg_sunken)
        .stroke(Stroke::new(1.0, p.divider))
        .corner_radius(Metrics::RADIUS_SM_C)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(p.text_secondary));
        });
}
