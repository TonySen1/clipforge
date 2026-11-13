use crate::{CoreError, Time};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Preview decode scale, relative to the preference's full resolution.
///
/// Scrubbing a 1080p source at full size means decoding and uploading 2 MP per
/// frame; at a quarter scale the same seek is several times faster, which is
/// what makes dragging feel immediate. "Full" is the honest option for judging
/// picture quality.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewQuality {
    Quarter,
    Third,
    Half,
    #[default]
    Full,
}

impl PreviewQuality {
    pub const ALL: [Self; 4] = [Self::Quarter, Self::Third, Self::Half, Self::Full];

    /// Multiplier applied to `preview_max_height`.
    pub fn scale(self) -> f32 {
        match self {
            Self::Quarter => 0.25,
            Self::Third => 1.0 / 3.0,
            Self::Half => 0.5,
            Self::Full => 1.0,
        }
    }

    /// Short label for the segmented control.
    pub fn label(self) -> &'static str {
        match self {
            Self::Quarter => "1/4",
            Self::Third => "1/3",
            Self::Half => "1/2",
            Self::Full => "1:1",
        }
    }

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Quarter => "quality.quarter",
            Self::Third => "quality.third",
            Self::Half => "quality.half",
            Self::Full => "quality.full",
        }
    }

    /// Effective long-edge cap for a full-resolution preference.
    ///
    /// Never drops below 120 px: below that the picture is unreadable and the
    /// user would think preview is broken rather than low quality.
    pub fn max_edge(self, full_height: u32) -> u32 {
        ((full_height as f32 * self.scale()).round() as u32).max(120)
    }
}

/// UI theme selection. `System` follows the operating system at runtime.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Light => "theme.light",
            Self::Dark => "theme.dark",
            Self::System => "theme.system",
        }
    }
}

/// UI language. The string forms are persisted in preferences.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locale {
    #[serde(rename = "en")]
    #[default]
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::En, Self::ZhCn];

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|locale| locale.code() == code)
    }

    /// Key for the endonym shown in the language picker.
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::En => "language.en",
            Self::ZhCn => "language.zh",
        }
    }
}

/// Application-level preferences, persisted outside the project/undo stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeMode,
    pub locale: Locale,
    pub last_import_dir: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    /// Preferred preview height cap; resolution is further capped by source.
    pub preview_max_height: u32,
    pub preview_quality: PreviewQuality,
    /// Drop empty spans between clips instead of rendering them as black and
    /// silence. Lossless export cannot represent gaps at all, so this also
    /// decides whether a gapped timeline can be exported losslessly.
    pub remove_gaps_on_export: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            locale: Locale::En,
            last_import_dir: None,
            ffmpeg_path: None,
            preview_max_height: 720,
            preview_quality: PreviewQuality::default(),
            remove_gaps_on_export: false,
        }
    }
}

impl Preferences {
    pub fn config_dir() -> Result<PathBuf, CoreError> {
        let dirs = directories::ProjectDirs::from("io", "ClipForge", "ClipForge")
            .ok_or(CoreError::NoConfigDir)?;
        Ok(dirs.config_dir().to_path_buf())
    }

    pub fn config_path() -> Result<PathBuf, CoreError> {
        Ok(Self::config_dir()?.join("preferences.json"))
    }

    /// Loads preferences, falling back to defaults on any read/parse failure.
    /// Failures are logged; the UI must never fail to start because of a
    /// corrupt preferences file.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(prefs) => prefs,
            Err(err) => {
                log::warn!("failed to load preferences, using defaults: {err}");
                Self::default()
            }
        }
    }

    pub fn try_load() -> Result<Self, CoreError> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, CoreError> {
        let data = std::fs::read_to_string(path).map_err(|source| CoreError::io(path, source))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn save(&self) -> Result<(), CoreError> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), CoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CoreError::io(parent, source))?;
        }

        let data = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|source| CoreError::io(&tmp, source))?;

        // `std::fs::rename` does not replace an existing file on Windows.
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(path).map_err(|source| CoreError::io(path, source))?;
        }

        std::fs::rename(&tmp, path).map_err(|source| CoreError::io(path, source))
    }
}

/// Formats a duration for the status bar; kept in core so tests can cover it
/// independently of the UI.
pub fn format_duration(duration: Time) -> String {
    duration.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_codes_round_trip() {
        for locale in Locale::ALL {
            assert_eq!(Locale::from_code(locale.code()), Some(locale));
        }
        assert_eq!(Locale::from_code("fr"), None);
    }

    #[test]
    fn preferences_round_trip() {
        let prefs = Preferences {
            theme: ThemeMode::Dark,
            locale: Locale::ZhCn,
            last_import_dir: Some(PathBuf::from("C:/media")),
            ffmpeg_path: None,
            preview_max_height: 720,
            preview_quality: PreviewQuality::default(),
            remove_gaps_on_export: true,
        };

        let dir = std::env::temp_dir().join("clipforge-prefs-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preferences.json");
        prefs.save_to(&path).unwrap();
        assert_eq!(Preferences::load_from(&path).unwrap(), prefs);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quality_maps_to_edge_caps() {
        assert_eq!(PreviewQuality::Full.max_edge(720), 720);
        assert_eq!(PreviewQuality::Half.max_edge(720), 360);
        assert_eq!(PreviewQuality::Third.max_edge(720), 240);
        assert_eq!(PreviewQuality::Quarter.max_edge(720), 180);
        // Never absurdly small.
        assert_eq!(PreviewQuality::Quarter.max_edge(320), 120);
    }

    #[test]
    fn unknown_or_missing_fields_use_defaults() {
        let parsed: Preferences = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(parsed.theme, ThemeMode::Dark);
        assert_eq!(parsed.locale, Locale::En);
        assert_eq!(parsed.preview_max_height, 720);
        assert_eq!(parsed.preview_quality, PreviewQuality::Full);
    }
}
