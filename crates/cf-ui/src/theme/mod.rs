//! Theme resolution and global egui style application.

pub mod tokens;

pub use tokens::{Metrics, Palette};

use cf_core::ThemeMode;
use egui::{Color32, Context, FontFamily, FontId, Stroke, TextStyle, ThemePreference, Visuals};
use std::collections::BTreeMap;

/// Resolves whether the effective theme is dark.
///
/// `System` asks the OS; detection failures fall back to dark, matching the
/// product's video-editing look.
pub fn resolve_dark(mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Light => false,
        ThemeMode::Dark => true,
        ThemeMode::System => match dark_light::detect() {
            Ok(dark_light::Mode::Light) => false,
            Ok(dark_light::Mode::Dark | dark_light::Mode::Unspecified) => true,
            Err(_) => true,
        },
    }
}

/// Applies fonts, spacing and visuals, returning the palette now in effect.
///
/// Both the light and dark style slots are filled with the same resolved
/// palette so egui can never flip to an unstyled theme mid-frame.
pub fn apply(ctx: &Context, mode: ThemeMode) -> Palette {
    let dark = resolve_dark(mode);
    let palette = Palette::of(dark);

    ctx.set_theme(if dark {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });

    let visuals = visuals(&palette);
    let text_styles = text_styles();
    ctx.all_styles_mut(|style| {
        style.text_styles = text_styles.clone();
        style.visuals = visuals.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = Metrics::CONTROL_HEIGHT;
        style.spacing.slider_width = 148.0;
        style.spacing.scroll.bar_width = 8.0;
    });

    palette
}

fn text_styles() -> BTreeMap<TextStyle, FontId> {
    [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        ),
    ]
    .into()
}

/// Maps the semantic palette onto egui's widget visuals.
pub fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.dark_mode = p.dark;
    v.panel_fill = p.bg_panel;
    v.window_fill = p.bg_elevated;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = Metrics::RADIUS_LG_C;
    v.menu_corner_radius = Metrics::RADIUS_MD_C;
    v.extreme_bg_color = p.bg_sunken;
    v.faint_bg_color = if p.dark {
        Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x08)
    } else {
        Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0x06)
    };
    v.text_edit_bg_color = Some(p.bg_sunken);
    v.code_bg_color = p.bg_sunken;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.error;
    v.weak_text_color = Some(p.text_secondary);
    v.weak_text_alpha = 1.0;

    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    let radius = Metrics::RADIUS_MD_C;
    let inactive_fg = Stroke::new(1.0, p.text_primary);

    v.widgets.noninteractive.bg_fill = p.bg_panel;
    v.widgets.noninteractive.weak_bg_fill = p.bg_panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.divider);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_secondary);
    v.widgets.noninteractive.corner_radius = Metrics::RADIUS_SM_C;
    v.widgets.noninteractive.expansion = 0.0;

    v.widgets.inactive.bg_fill = p.bg_elevated;
    v.widgets.inactive.weak_bg_fill = p.bg_sunken;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = inactive_fg;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.inactive.expansion = 0.0;

    v.widgets.hovered.bg_fill = p.bg_hover;
    v.widgets.hovered.weak_bg_fill = p.accent_soft;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, p.text_primary);
    v.widgets.hovered.corner_radius = radius;
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5, p.accent);
    v.widgets.active.corner_radius = radius;
    v.widgets.active.expansion = 0.0;

    v.widgets.open.bg_fill = p.bg_elevated;
    v.widgets.open.weak_bg_fill = p.bg_elevated;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text_primary);
    v.widgets.open.corner_radius = radius;
    v.widgets.open.expansion = 0.0;

    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_modes_resolve_without_system_lookup() {
        assert!(!resolve_dark(ThemeMode::Light));
        assert!(resolve_dark(ThemeMode::Dark));
    }

    #[test]
    fn visuals_use_palette_surfaces() {
        let light = visuals(&Palette::LIGHT);
        assert!(!light.dark_mode);
        assert_eq!(light.panel_fill, Palette::LIGHT.bg_panel);

        let dark = visuals(&Palette::DARK);
        assert!(dark.dark_mode);
        assert_eq!(dark.panel_fill, Palette::DARK.bg_panel);
    }
}
