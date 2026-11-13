//! ClipForge design system, localization and reusable egui widgets.
//!
//! The UI layer never spawns processes and never mutates the project directly;
//! it emits actions for the app state layer to apply.

rust_i18n::i18n!("../../locales", fallback = "en");

pub mod dialogs;
pub mod fonts;
pub mod i18n;
pub mod icons;
pub mod theme;
pub mod widgets;

pub use widgets::TransportAction;
