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
