//! Design tokens: the single source of truth for colors, radii and spacing.
//!
//! No widget or dialog may hard-code a color. If a new semantic color is
//! needed, add it here and map it in both palettes.

use egui::{Color32, CornerRadius};

/// Radii and paddings shared by every widget.
pub struct Metrics;

impl Metrics {
    pub const RADIUS_XL: u8 = 18;
    pub const RADIUS_LG: u8 = 14;
    pub const RADIUS_MD: u8 = 10;
    pub const RADIUS_SM: u8 = 7;
    pub const RADIUS_XS: u8 = 4;

    pub const TOOLBAR_HEIGHT: f32 = 54.0;
    pub const STATUS_HEIGHT: f32 = 26.0;
    pub const TIMELINE_DEFAULT_HEIGHT: f32 = 232.0;
    pub const TRANSPORT_HEIGHT: f32 = 32.0;
    pub const GAP: f32 = 10.0;
    pub const CONTROL_HEIGHT: f32 = 30.0;

    pub const RADIUS_LG_C: CornerRadius = CornerRadius::same(Self::RADIUS_LG);
    pub const RADIUS_MD_C: CornerRadius = CornerRadius::same(Self::RADIUS_MD);
    pub const RADIUS_SM_C: CornerRadius = CornerRadius::same(Self::RADIUS_SM);
    pub const RADIUS_XS_C: CornerRadius = CornerRadius::same(Self::RADIUS_XS);
}

/// Semantic color palette for one theme.
///
/// Keep the field names semantic (`bg_panel`, `text_muted`, `clip_video`) and
/// not literal (`gray_20`, `blue_500`) so both themes stay in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub dark: bool,

    // Surfaces, from the window backdrop up to elevated popups.
    pub bg_base: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub bg_sunken: Color32,
    pub bg_hover: Color32,
    /// Outer preview surface. Follows the theme so the chrome stays cohesive.
    pub bg_stage: Color32,
    /// The 16:9 frame well. Deliberately near-black in both themes: video is
    /// judged against black, and a light well around a dark clip reads broken.
    pub bg_stage_inset: Color32,
    pub track_bg: Color32,

    // Text.
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub text_on_accent: Color32,

    // Brand and state.
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub cyan: Color32,
    pub violet: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub divider: Color32,

    // Timeline.
    pub clip_video: Color32,
    pub clip_audio: Color32,
    pub clip_selected: Color32,
    pub playhead: Color32,

    pub warning: Color32,
    pub error: Color32,
    pub success: Color32,

    // Scrims and text that sits on the always-dark stage or stage overlay.
    // These are deliberately identical in both themes: the stage is dark even
    // in light mode, so on-stage text must not follow the panel palette.
    pub scrim_modal: Color32,
    pub scrim_overlay: Color32,
    pub scrim_chip: Color32,
    pub shadow: Color32,
    pub on_stage_primary: Color32,
    pub on_stage_muted: Color32,
    /// Text drawn on a clip block. Clip fills are saturated in dark mode and
    /// pale in light mode, so this is the one place where light mode wants
    /// *darker* text than dark mode.
    pub on_clip: Color32,
    /// Highlight painted along the top of a progress fill.
    pub progress_highlight: Color32,
    /// Lighter overlay used for the moving stripes inside a progress fill.
    pub progress_stripe: Color32,
}

impl Palette {
    pub const LIGHT: Self = Self {
        dark: false,
        bg_base: Color32::from_rgb(0xEE, 0xF1, 0xF6),
        bg_panel: Color32::from_rgb(0xF8, 0xF9, 0xFC),
        bg_elevated: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        bg_sunken: Color32::from_rgb(0xE4, 0xE8, 0xEF),
        bg_hover: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        bg_stage: Color32::from_rgb(0xDF, 0xE5, 0xEE),
        bg_stage_inset: Color32::from_rgb(0x08, 0x0B, 0x11),
        track_bg: Color32::from_rgb(0xE9, 0xED, 0xF4),
        text_primary: Color32::from_rgb(0x16, 0x1B, 0x22),
        text_secondary: Color32::from_rgb(0x57, 0x60, 0x6C),
        text_muted: Color32::from_rgb(0x8A, 0x94, 0xA3),
        text_on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        accent: Color32::from_rgb(0x3E, 0x7D, 0xC4),
        accent_hover: Color32::from_rgb(0x2F, 0x6B, 0xB0),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x3E, 0x7D, 0xC4, 0x24),
        cyan: Color32::from_rgb(0x12, 0x8C, 0xA8),
        violet: Color32::from_rgb(0x6F, 0x5B, 0xC8),
        border: Color32::from_rgb(0xD6, 0xDC, 0xE5),
        border_strong: Color32::from_rgb(0xB9, 0xC2, 0xCF),
        divider: Color32::from_rgb(0xE4, 0xE8, 0xEF),
        clip_video: Color32::from_rgb(0xBF, 0xD4, 0xF2),
        clip_audio: Color32::from_rgb(0xC2, 0xE8, 0xDC),
        clip_selected: Color32::from_rgb(0x3E, 0x7D, 0xC4),
        playhead: Color32::from_rgb(0xE0, 0x44, 0x3E),
        warning: Color32::from_rgb(0xB2, 0x6A, 0x00),
        error: Color32::from_rgb(0xC6, 0x28, 0x28),
        success: Color32::from_rgb(0x2E, 0x7D, 0x32),
        // A light theme needs a much lighter veil; black-90% over a light
        // surface reads as a broken theme rather than a modal backdrop.
        scrim_modal: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 64),
        scrim_overlay: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 150),
        scrim_chip: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 150),
        shadow: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 90),
        on_stage_primary: Color32::from_rgb(0xC8, 0xD3, 0xE2),
        on_stage_muted: Color32::from_rgb(0x74, 0x81, 0x94),
        on_clip: Color32::from_rgb(0x1A, 0x2A, 0x3D),
        progress_highlight: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 38),
        progress_stripe: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 46),
    };

    pub const DARK: Self = Self {
        dark: true,
        bg_base: Color32::from_rgb(0x0B, 0x0E, 0x13),
        bg_panel: Color32::from_rgb(0x12, 0x16, 0x1D),
        bg_elevated: Color32::from_rgb(0x1A, 0x1F, 0x29),
        bg_sunken: Color32::from_rgb(0x07, 0x09, 0x0D),
        bg_hover: Color32::from_rgb(0x22, 0x2A, 0x36),
        bg_stage: Color32::from_rgb(0x05, 0x07, 0x0A),
        bg_stage_inset: Color32::from_rgb(0x00, 0x00, 0x00),
        track_bg: Color32::from_rgb(0x0F, 0x14, 0x1C),
        text_primary: Color32::from_rgb(0xE8, 0xED, 0xF5),
        text_secondary: Color32::from_rgb(0xA3, 0xAD, 0xBC),
        text_muted: Color32::from_rgb(0x6D, 0x78, 0x89),
        text_on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        accent: Color32::from_rgb(0x4F, 0x9C, 0xFF),
        accent_hover: Color32::from_rgb(0x74, 0xB4, 0xFF),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x4F, 0x9C, 0xFF, 0x29),
        cyan: Color32::from_rgb(0x63, 0xE6, 0xFF),
        violet: Color32::from_rgb(0xA7, 0x8B, 0xFA),
        border: Color32::from_rgb(0x25, 0x2D, 0x3A),
        border_strong: Color32::from_rgb(0x3A, 0x45, 0x56),
        divider: Color32::from_rgb(0x1C, 0x23, 0x30),
        clip_video: Color32::from_rgb(0x2B, 0x4A, 0x73),
        clip_audio: Color32::from_rgb(0x1F, 0x4A, 0x42),
        clip_selected: Color32::from_rgb(0x4F, 0x9C, 0xFF),
        playhead: Color32::from_rgb(0xFF, 0x5B, 0x55),
        warning: Color32::from_rgb(0xE0, 0xA0, 0x30),
        error: Color32::from_rgb(0xFF, 0x6B, 0x6B),
        success: Color32::from_rgb(0x4C, 0xD0, 0x7D),
        scrim_modal: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 150),
        scrim_overlay: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 150),
        scrim_chip: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 150),
        shadow: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 90),
        on_stage_primary: Color32::from_rgb(0xC8, 0xD3, 0xE2),
        on_stage_muted: Color32::from_rgb(0x74, 0x81, 0x94),
        on_clip: Color32::from_rgb(0xE8, 0xED, 0xF5),
        progress_highlight: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 38),
        progress_stripe: Color32::from_rgba_unmultiplied_const(0xFF, 0xFF, 0xFF, 46),
    };

    pub const fn of(dark: bool) -> Self {
        if dark {
            Self::DARK
        } else {
            Self::LIGHT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_distinct_and_complete() {
        let light = Palette::LIGHT;
        let dark = Palette::DARK;
        assert_ne!(light.bg_base, dark.bg_base);
        assert_ne!(light.text_primary, dark.text_primary);
        assert_ne!(light.accent, dark.accent);
        assert!(!light.dark);
        assert!(dark.dark);
        assert_eq!(Palette::of(true), Palette::DARK);
        assert_eq!(Palette::of(false), Palette::LIGHT);
    }
}
