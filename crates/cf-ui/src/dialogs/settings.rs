//! Settings dialog: appearance and language, applied immediately.

use crate::i18n::tr;
use crate::theme::{Metrics, Palette};
use crate::widgets::controls::subtle_button;
use cf_core::{Locale, Preferences, ThemeMode};
use egui::{
    Align, Button, Color32, Context, Frame, Id, Margin, Modal, RichText, ScrollArea, Stroke, Ui,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Close,
    Changed,
}

pub fn show(ctx: &Context, p: &Palette, prefs: &mut Preferences) -> SettingsAction {
    let before = prefs.clone();
    let mut close = false;

    let frame = Frame::new()
        .fill(p.bg_elevated)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(Metrics::RADIUS_LG_C)
        .inner_margin(Margin::same(22))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 28,
            spread: 0,
            color: p.shadow,
        });

    let response = Modal::new(Id::unique("clipforge_settings"))
        .frame(frame)
        .backdrop_color(crate::dialogs::about::backdrop_color(p))
        .show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(
                RichText::new(tr("settings.title"))
                    .size(19.0)
                    .strong()
                    .color(p.text_primary),
            );
            ui.add_space(14.0);

            ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                section(ui, p, &tr("settings.appearance"));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for mode in ThemeMode::ALL {
                        if option(ui, p, prefs.theme == mode, &tr(mode.i18n_key())).clicked() {
                            prefs.theme = mode;
                        }
                    }
                });

                ui.add_space(16.0);
                divider(ui, p);
                ui.add_space(12.0);

                section(ui, p, &tr("settings.language"));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for locale in Locale::ALL {
                        if option(ui, p, prefs.locale == locale, &tr(locale.i18n_key())).clicked() {
                            prefs.locale = locale;
                        }
                    }
                });
            });

            ui.add_space(18.0);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if subtle_button(ui, p, tr("settings.close")).clicked() {
                    close = true;
                }
            });
        });

    if close || response.should_close() {
        SettingsAction::Close
    } else if prefs.theme != before.theme || prefs.locale != before.locale {
        SettingsAction::Changed
    } else {
        SettingsAction::None
    }
}

fn section(ui: &mut Ui, p: &Palette, title: &str) {
    ui.label(RichText::new(title).size(11.0).strong().color(p.text_muted));
}

fn divider(ui: &mut Ui, p: &Palette) {
    let width = ui.available_width();
    ui.add_space(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.divider);
    ui.add_space(1.0);
}

fn option(ui: &mut Ui, p: &Palette, selected: bool, label: &str) -> egui::Response {
    let fill = if selected {
        p.accent_soft
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected {
        Stroke::new(1.0, p.accent)
    } else {
        Stroke::new(1.0, p.border)
    };
    let text = if selected { p.accent } else { p.text_secondary };

    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = fill;
        w.inactive.bg_fill = fill;
        w.inactive.bg_stroke = stroke;
        w.inactive.fg_stroke = Stroke::new(1.0, text);
        w.hovered.weak_bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_stroke = stroke;
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.accent);

        ui.add(
            Button::new(RichText::new(label).color(text).size(13.0))
                .frame(true)
                .min_size(egui::vec2(0.0, 32.0)),
        )
    })
    .inner
}
