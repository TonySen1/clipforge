//! Font setup.
//!
//! Product requirement: every panel and window uses **HarmonyOS Sans**.
//! Resolution order:
//!
//! 1. `CLIPFORGE_FONT` environment variable (a direct path to a `.ttf`/`.otf`).
//! 2. `<exe dir>/fonts/HarmonyOS_Sans_SC_Regular.ttf` for portable bundles.
//! 3. The workspace `assets/fonts/` copy (development builds).
//! 4. An installed `HarmonyOS_Sans*` face under the OS font directories.
//! 5. **Microsoft YaHei** (`msyh.ttc`) as the required CJK fallback.
//! 6. egui's bundled fonts plus eframe's system font provider.
//!
//! The chosen font is installed as the primary family for both proportional
//! and monospace text so no widget falls back silently to a different face.

use egui::{Context, FontData, FontDefinitions, FontFamily};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What actually got loaded, for logs and the about dialog.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FontSource {
    HarmonyOsSans,
    MicrosoftYaHei,
    BuiltIn,
}

impl FontSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::HarmonyOsSans => "HarmonyOS Sans",
            Self::MicrosoftYaHei => "Microsoft YaHei",
            Self::BuiltIn => "egui built-in",
        }
    }
}

pub fn install(ctx: &Context) -> FontSource {
    let mut fonts = FontDefinitions::default();
    let mut primary: Vec<String> = Vec::new();
    let mut source = FontSource::BuiltIn;

    if let Some(path) = find_harmony() {
        match std::fs::read(&path) {
            Ok(bytes) => {
                log::info!("using HarmonyOS Sans from {}", path.display());
                insert(&mut fonts, "harmony", bytes, 0);
                primary.push("harmony".to_owned());
                source = FontSource::HarmonyOsSans;
            }
            Err(err) => {
                log::warn!("failed to read {}: {err}", path.display());
            }
        }
    }

    // Microsoft YaHei is always registered as a fallback when present, so
    // glyphs missing from HarmonyOS Sans still render with the intended face.
    if let Some(path) = find_yahei() {
        if let Ok(bytes) = std::fs::read(&path) {
            log::info!("adding Microsoft YaHei fallback from {}", path.display());
            insert(&mut fonts, "yahei", bytes, 0);
            primary.push("yahei".to_owned());
            if source == FontSource::BuiltIn {
                source = FontSource::MicrosoftYaHei;
            }
        }
    }

    // Primary fonts come first; egui's defaults and the system provider stay
    // behind them for symbols and any script we did not cover.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for name in primary.iter().rev() {
            list.insert(0, name.clone());
        }
    }

    ctx.set_fonts(fonts);
    source
}

fn insert(fonts: &mut FontDefinitions, name: &str, bytes: Vec<u8>, index: u32) {
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert(name.to_owned(), Arc::new(data));
}

fn find_harmony() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLIPFORGE_FONT").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let portable = dir.join("fonts").join("HarmonyOS_Sans_SC_Regular.ttf");
            if portable.is_file() {
                return Some(portable);
            }
        }
    }

    // Development runs read the workspace copy directly.
    let dev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/fonts/HarmonyOS_Sans_SC_Regular.ttf");
    if dev.is_file() {
        return Some(dev);
    }

    find_installed(&[
        "HarmonyOS_Sans_SC_Regular.ttf",
        "HarmonyOS_Sans_SC_Medium.ttf",
        "HarmonyOS_Sans_SC_Light.ttf",
        "HarmonyOS_Sans_Regular.ttf",
    ])
}

fn find_yahei() -> Option<PathBuf> {
    find_installed(&["msyh.ttc", "msyh.ttf"])
}

fn find_installed(preferred: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    #[cfg(windows)]
    {
        dirs.push(PathBuf::from(r"C:\Windows\Fonts"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Fonts"),
            );
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library").join("Fonts"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("fonts"),
            );
        }
    }

    for dir in dirs {
        for name in preferred {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    // Fonts installed per-user on Windows may live under a hashed name; fall
    // back to a shallow directory scan of the first candidate dir only.
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts");
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut matches: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                            e.eq_ignore_ascii_case("ttf")
                                || e.eq_ignore_ascii_case("ttc")
                                || e.eq_ignore_ascii_case("otf")
                        })
                    })
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.to_lowercase().contains("harmony"))
                    })
                    .collect();
                matches.sort();
                if let Some(path) = matches.into_iter().next() {
                    return Some(path);
                }
            }
        }
    }

    None
}
