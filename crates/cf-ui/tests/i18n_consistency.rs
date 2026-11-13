//! Guards the translation files against drift.
//!
//! Two failure modes this catches:
//!
//! * a key that exists in one locale but not the other, which renders as a raw
//!   `settings.title` in the UI;
//! * a `tr("...")` call in the source with no entry in any locale file.
//!
//! The second check scans the source tree, so it also catches typos added in
//! code without touching the YAML.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn locales_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../locales")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Flattens `a: { b: { c: x } }` into `a.b.c`.
fn flatten(yaml: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut prefix: Vec<String> = Vec::new();

    for line in yaml.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }

        let indent = trimmed.len() - trimmed.trim_start().len();
        let depth = indent / 2;

        let Some((key, value)) = trimmed.trim_start().split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        prefix.truncate(depth);
        prefix.push(key.to_owned());

        if !value.trim().is_empty() {
            keys.insert(prefix.join("."));
        }
    }

    keys
}

fn load(name: &str) -> BTreeSet<String> {
    let path = locales_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    flatten(&text)
}

#[test]
fn every_locale_defines_the_same_keys() {
    let en = load("en.yml");
    let zh = load("zh-CN.yml");

    let only_en: Vec<_> = en.difference(&zh).cloned().collect();
    let only_zh: Vec<_> = zh.difference(&en).cloned().collect();

    assert!(only_en.is_empty(), "keys missing from zh-CN: {only_en:?}");
    assert!(only_zh.is_empty(), "keys missing from en: {only_zh:?}");
}

/// Keys the app looks up through a method rather than a literal.
///
/// `ThemeMode::i18n_key`, `Locale::i18n_key`, `ExportMode::*`,
/// `ExportBlocker::i18n_key`, `CompatIssue::field_i18n_key` and the edit
/// history labels all resolve to strings defined in Rust.
const DYNAMIC_KEYS: &[&str] = &[
    // ThemeMode
    "theme.light",
    "theme.dark",
    "theme.system",
    // Locale
    "language.en",
    "language.zh",
    // Edit history labels
    "edit.split",
    "edit.move",
    "edit.trim",
    "edit.delete",
    "edit.merge",
    // ExportMode
    "export.mode_lossless",
    "export.mode_precise",
    "export.mode_lossless_hint",
    "export.mode_precise_hint",
    // ExportBlocker
    "export.blocker.keyframes",
    "export.blocker.empty_range",
    "export.blocker.output_is_input",
    "export.blocker.engine_missing",
    "export.blocker.missing_source",
    "export.blocker.nothing",
    "export.blocker.gaps",
    "export.blocker.incompatible",
    "export.blocker.container",
    "export.blocker.range_empty",
    // CompatIssue fields
    "export.field.video_codec",
    "export.field.profile",
    "export.field.width",
    "export.field.height",
    "export.field.pix_fmt",
    "export.field.frame_rate",
    "export.field.sar",
    "export.field.audio_codec",
    "export.field.sample_rate",
    "export.field.channels",
    "export.field.other",
    // Codec names
    "export.codec.h264",
    "export.codec.h265",
    "export.codec.vp9",
];

#[test]
fn dynamic_keys_exist_in_both_locales() {
    let en = load("en.yml");
    let zh = load("zh-CN.yml");

    let missing_en: Vec<_> = DYNAMIC_KEYS
        .iter()
        .filter(|key| !en.contains(**key))
        .collect();
    let missing_zh: Vec<_> = DYNAMIC_KEYS
        .iter()
        .filter(|key| !zh.contains(**key))
        .collect();

    assert!(missing_en.is_empty(), "missing in en.yml: {missing_en:?}");
    assert!(
        missing_zh.is_empty(),
        "missing in zh-CN.yml: {missing_zh:?}"
    );
}

/// Every `tr("literal")` in the workspace must resolve.
#[test]
fn literal_tr_calls_have_translations() {
    let en = load("en.yml");
    let zh = load("zh-CN.yml");

    let mut missing: BTreeSet<String> = BTreeSet::new();
    walk(&workspace_root().join("crates"), &mut |file| {
        if file.extension().and_then(|e| e.to_str()) != Some("rs") {
            return;
        }
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(_) => return,
        };
        for key in literal_keys(&text) {
            if !en.contains(&key) || !zh.contains(&key) {
                missing.insert(key);
            }
        }
    });

    assert!(missing.is_empty(), "untranslated tr() keys: {missing:?}");
}

/// Extracts string literals passed to `tr(`.
fn literal_keys(source: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;

    while index + 3 < bytes.len() {
        // Look for `tr("`, but only when `tr` is a whole identifier: that
        // rejects look-alikes such as `my_tr(` and `not_tr(`.
        let boundary = index == 0
            || !matches!(bytes[index - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_');
        if boundary && &bytes[index..index + 3] == b"tr(" && bytes[index + 3] == b'"' {
            let start = index + 4;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'"' {
                if bytes[end] == b'\\' {
                    end += 1;
                }
                end += 1;
            }
            if end < bytes.len() {
                if let Ok(key) = std::str::from_utf8(&bytes[start..end]) {
                    keys.push(key.to_owned());
                }
            }
            index = end;
        }
        index += 1;
    }

    keys
}

fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str());
            // `target` holds build output; `tests` holds fixtures that mention
            // `tr("...")` in their own assertions.
            if matches!(name, Some("target") | Some("tests")) {
                continue;
            }
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn flatten_builds_dotted_paths() {
        let yaml = "app:\n  name: ClipForge\n  tagline: Hi\nsettings:\n  title: Settings\n";
        let keys = flatten(yaml);
        assert!(keys.contains("app.name"));
        assert!(keys.contains("app.tagline"));
        assert!(keys.contains("settings.title"));
        assert!(!keys.contains("app"));
    }

    #[test]
    fn flatten_skips_comments_and_blanks() {
        let yaml = "# a comment\n\napp:\n  name: X\n";
        let keys = flatten(yaml);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("app.name"));
    }

    #[test]
    fn extractor_finds_literals_only() {
        let source = r#"
            let a = tr("a.b");
            let b = tr("b.c");
            let dynamic = tr(key_var);
            let other = my_tr("x.y");
        "#;
        let keys = literal_keys(source);
        assert_eq!(keys, vec!["a.b".to_owned(), "b.c".to_owned()]);
    }
}

/// Widget code must not invent colors; every color comes from `tokens.rs`.
///
/// This walks the UI source and rejects `Color32::from_rgb` /
/// `from_black_alpha` outside the token module. It is intentionally coarse —
/// it catches the common regression (a quick fix-color inline) without
/// needing to parse Rust.
#[test]
fn no_hardcoded_colors_outside_tokens() {
    let mut offenders = Vec::new();

    walk(&workspace_root().join("crates"), &mut |file| {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(_) => return,
        };
        let relative = file.to_string_lossy().replace('\\', "/");

        // The token tables and the Visuals mapping are the only places where
        // literal colors are expected.
        if relative.ends_with("theme/tokens.rs") || relative.ends_with("theme/mod.rs") {
            return;
        }
        let _ = name;

        for (line_number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains("Color32::from_rgb(")
                || line.contains("Color32::from_black_alpha(")
                || line.contains("Color32::from_white_alpha(")
            {
                offenders.push(format!("{}:{}", relative, line_number + 1));
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "hardcoded colors outside the token module: {offenders:?}"
    );
}
