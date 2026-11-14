//! `cargo xtask package` — assemble the portable and installed distributions.
//!
//! Two variants are produced, because bundling FFmpeg makes the download ~150 MB
//! larger and not everyone wants that:
//!
//! * **Full** — `clipforge-<ver>-<target>/` with FFmpeg in `bin/`. Works on a
//!   clean machine.
//! * **Slim** — `clipforge-<ver>-<target>-slim/` without FFmpeg. Needs an
//!   existing install on `PATH`, in `bin/`, or via the preference file.
//!
//! Each variant ships `clipforge.exe`, `fonts/`, `README.txt`, `LICENSE` and
//! `THIRD_PARTY.md`. The full variant adds the GPL FFmpeg build, so
//! ClipForge itself stays GPL-3.0-or-later (see THIRD_PARTY.md).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{repo_root, run_cargo};

/// Which distribution to assemble.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Variant {
    /// Includes the FFmpeg build in `bin/`.
    Full,
    /// Leaves FFmpeg to the user's machine.
    Slim,
}

impl Variant {
    /// Suffix appended to the folder and archive names. The full build keeps
    /// the plain name so existing links keep working.
    fn suffix(self) -> &'static str {
        match self {
            Self::Full => "",
            Self::Slim => "-slim",
        }
    }

    /// Human label for logs and the README.
    fn label(self) -> &'static str {
        match self {
            Self::Full => "full (FFmpeg included)",
            Self::Slim => "slim (no FFmpeg)",
        }
    }
}

/// Builds the release binary once, then assembles both variants.
pub fn package() -> Result<()> {
    let root = repo_root();
    let version = workspace_version(&root)?;
    let target = host_target()?;

    println!("packaging ClipForge {version} for {target}");

    run_cargo(&root, &["build", "--release", "-p", "cf-app"])?;

    let exe_name = if cfg!(windows) {
        "clipforge.exe"
    } else {
        "clipforge"
    };
    let built = root.join("target").join("release").join(exe_name);
    if !built.is_file() {
        bail!(
            "expected a release binary at {} — did the build fail?",
            built.display()
        );
    }

    // Locate FFmpeg once; a missing install only degrades the full variant to
    // a slim one, with a loud warning.
    let ffmpeg_source = find_ffmpeg_install();
    if ffmpeg_source.is_none() {
        println!(
            "warning: no FFmpeg install found; only the slim variant will be built.\n\
             Set CLIPFORGE_FFMPEG_DIR, or place a build in vendor/ffmpeg/bin."
        );
    }

    let mut variants = vec![Variant::Slim];
    if ffmpeg_source.is_some() {
        variants.insert(0, Variant::Full);
    }

    for variant in variants {
        assemble(
            &root,
            &built,
            exe_name,
            &version,
            &target,
            variant,
            &ffmpeg_source,
        )?;
    }

    Ok(())
}

/// Assembles one variant into its own folder, archive and (for the full build)
/// installer.
fn assemble(
    root: &Path,
    built: &Path,
    exe_name: &str,
    version: &str,
    target: &str,
    variant: Variant,
    ffmpeg_source: &Option<PathBuf>,
) -> Result<()> {
    println!("\n=== {} ===", variant.label());

    let out_dir = root
        .join("dist")
        .join(format!("clipforge-{version}-{target}{}", variant.suffix()));
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)
            .with_context(|| format!("clearing {}", out_dir.display()))?;
    }
    std::fs::create_dir_all(out_dir.join("fonts"))
        .with_context(|| format!("creating {}", out_dir.display()))?;

    std::fs::copy(built, out_dir.join(exe_name))
        .with_context(|| format!("copying the binary to {}", out_dir.display()))?;

    // The font is what makes the UI look right; without it the app falls back
    // to a system face and the layout shifts.
    let font = root
        .join("assets")
        .join("fonts")
        .join("HarmonyOS_Sans_SC_Regular.ttf");
    if font.is_file() {
        std::fs::copy(
            &font,
            out_dir.join("fonts").join("HarmonyOS_Sans_SC_Regular.ttf"),
        )
        .context("copying the bundled font")?;
        let license = root
            .join("assets")
            .join("fonts")
            .join("HarmonyOS_Sans_LICENSE.txt");
        if license.is_file() {
            std::fs::copy(
                &license,
                out_dir.join("fonts").join("HarmonyOS_Sans_LICENSE.txt"),
            )
            .context("copying the font license")?;
        }
    } else {
        println!(
            "warning: {} is missing, the app will fall back to a system font",
            font.display()
        );
    }

    for name in ["README.md", "THIRD_PARTY.md", "LICENSE"] {
        let source = root.join(name);
        if source.is_file() {
            std::fs::copy(&source, out_dir.join(name))
                .with_context(|| format!("copying {name}"))?;
        }
    }

    let bundled = match (variant, ffmpeg_source) {
        (Variant::Full, Some(source)) => {
            bundle_ffmpeg(source, &out_dir)?;
            verify_bundle(&out_dir.join("bin"))?;
            Some(source.to_path_buf())
        }
        _ => None,
    };
    std::fs::write(out_dir.join("README.txt"), quickstart(version, bundled))?;

    println!("wrote {}", out_dir.display());
    list_dir(&out_dir, "  ")?;

    if cfg!(windows) {
        build_portable_zip(root, &out_dir, version, variant)?;
        // One installer is enough: the slim build is the "bring your own
        // FFmpeg" option and is portable-only on purpose.
        if variant == Variant::Full {
            match build_installer(root, &out_dir, version) {
                Ok(Some(path)) => println!("installer: {}", path.display()),
                Ok(None) => println!(
                    "skipping the installer: NSIS (makensis) was not found. \
                     Install it from https://nsis.sourceforge.io/ and re-run `cargo xtask package`."
                ),
                Err(err) => println!("installer build failed: {err:#}"),
            }
        }
    }

    Ok(())
}

/// Zips the portable folder next to it, so the release page has an
/// extract-and-run option that needs no installer.
fn build_portable_zip(
    root: &Path,
    out_dir: &Path,
    version: &str,
    variant: Variant,
) -> Result<PathBuf> {
    let name = out_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("clipforge-{version}{}", variant.suffix()));
    let zip_path = root.join("dist").join(format!("{name}.zip"));

    if zip_path.exists() {
        std::fs::remove_file(&zip_path)
            .with_context(|| format!("replacing {}", zip_path.display()))?;
    }

    // PowerShell has Compress-Archive everywhere Windows ships; adding a zip
    // crate for one call is not worth the dependency.
    let script = format!(
        "Compress-Archive -Path '{}' -DestinationPath '{}' -Force",
        out_dir.display(),
        zip_path.display()
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .context("running PowerShell to create the zip")?;
    if !status.success() {
        bail!("Compress-Archive failed for {}", zip_path.display());
    }

    println!("portable zip: {}", zip_path.display());
    Ok(zip_path)
}

/// Builds an NSIS installer when `makensis` is on `PATH` (or in the default
/// install location). Returns `None` when the toolchain is absent so the
/// portable build still succeeds.
fn build_installer(root: &Path, out_dir: &Path, version: &str) -> Result<Option<PathBuf>> {
    let Some(makensis) = find_makensis() else {
        return Ok(None);
    };

    let script_path = root.join("dist").join("clipforge-installer.nsi");
    let installer_path = root
        .join("dist")
        .join(format!("ClipForge-{version}-win64-setup.exe"));
    let icon = root.join("assets").join("icons").join("clipforge.ico");

    // NSIS wants backslashes and is unhappy with forward slashes in paths.
    let win = |path: &Path| path.display().to_string().replace('/', "\\");

    let script = format!(
        r#"Unicode true
SetCompressor /SOLID lzma
Name "ClipForge {version}"
OutFile "{out}"
InstallDir "$LOCALAPPDATA\Programs\ClipForge"
InstallDirRegKey HKCU "Software\ClipForge" "InstallDir"
RequestExecutionLevel user

!include "MUI2.nsh"
!define MUI_ABORTWARNING

; `MUI_FINISHPAGE_RUN_TEXT` is expanded at define time, before the language
; tables exist, so it carries a literal bilingual label instead of $(RunApp).
!define MUI_FINISHPAGE_RUN "$INSTDIR\clipforge.exe"
!define MUI_FINISHPAGE_RUN_TEXT "启动 ClipForge / Launch ClipForge"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; MUI requires the language macros after the page macros and before any
; `Section` that references `$(...)` names.
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

LangString SecMain          ${{LANG_SIMPCHINESE}} "ClipForge（必需）"
LangString SecMain          ${{LANG_ENGLISH}}      "ClipForge (required)"
LangString SecMainDesc      ${{LANG_SIMPCHINESE}} "程序文件、卸载程序与系统中的应用条目。"
LangString SecMainDesc      ${{LANG_ENGLISH}}      "Program files, the uninstaller and the system entry."
LangString SecStartMenu     ${{LANG_SIMPCHINESE}} "开始菜单快捷方式"
LangString SecStartMenu     ${{LANG_ENGLISH}}      "Start menu shortcuts"
LangString SecStartMenuDesc ${{LANG_SIMPCHINESE}} "在开始菜单中创建 ClipForge 文件夹，包含启动与卸载。"
LangString SecStartMenuDesc ${{LANG_ENGLISH}}      "Creates a ClipForge folder in the Start menu with launch and uninstall entries."
LangString SecDesktop       ${{LANG_SIMPCHINESE}} "桌面快捷方式"
LangString SecDesktop       ${{LANG_ENGLISH}}      "Desktop shortcut"
LangString SecDesktopDesc   ${{LANG_SIMPCHINESE}} "在桌面创建 ClipForge 图标（默认不勾选）。"
LangString SecDesktopDesc   ${{LANG_ENGLISH}}      "Puts a ClipForge icon on the desktop (off by default)."

Section "$(SecMain)" SecMain
  SetOutPath "$INSTDIR"
  File /r "{src}\*.*"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\ClipForge" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "DisplayName" "ClipForge"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "DisplayVersion" "{version}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "DisplayIcon" "$INSTDIR\clipforge.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "NoModify" "1"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge" "NoRepair" "1"
SectionEnd

Section "$(SecStartMenu)" SecStartMenu
  SetShellVarContext current
  CreateDirectory "$SMPROGRAMS\ClipForge"
  CreateShortcut "$SMPROGRAMS\ClipForge\ClipForge.lnk" "$INSTDIR\clipforge.exe"
  CreateShortcut "$SMPROGRAMS\ClipForge\Uninstall.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "$(SecDesktop)" SecDesktop
  SetShellVarContext current
  CreateShortcut "$DESKTOP\ClipForge.lnk" "$INSTDIR\clipforge.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${{SecMain}} "$(SecMainDesc)"
  !insertmacro MUI_DESCRIPTION_TEXT ${{SecStartMenu}} "$(SecStartMenuDesc)"
  !insertmacro MUI_DESCRIPTION_TEXT ${{SecDesktop}} "$(SecDesktopDesc)"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$DESKTOP\ClipForge.lnk"
  Delete "$SMPROGRAMS\ClipForge\ClipForge.lnk"
  Delete "$SMPROGRAMS\ClipForge\Uninstall.lnk"
  RMDir "$SMPROGRAMS\ClipForge"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipForge"
  DeleteRegKey HKCU "Software\ClipForge"
  RMDir /r "$INSTDIR"
SectionEnd
"#,
        out = win(&installer_path),
        src = win(out_dir),
    );

    if icon.is_file() {
        // MUI2 owns the installer/uninstaller icons through `MUI_ICON` and
        // `MUI_UNICON`; a bare `Icon` line is overridden by the page macros, so
        // the installer kept the stock NSIS icon. These must be defined after
        // `!include MUI2.nsh` and before the first page macro.
        let with_icon = script.replace(
            "!insertmacro MUI_PAGE_WELCOME",
            &format!(
                "!define MUI_ICON \"{icon}\"\n!define MUI_UNICON \"{icon}\"\n\
                 !insertmacro MUI_PAGE_WELCOME",
                icon = win(&icon)
            ),
        );
        // The script contains Chinese LangStrings; without a BOM makensis
        // reads it as the ANSI code page and aborts on the first non-ASCII
        // byte.
        write_utf8_bom(&script_path, &with_icon)?;
    } else {
        write_utf8_bom(&script_path, &script)?;
    }

    let status = Command::new(&makensis)
        .arg(&script_path)
        .current_dir(root)
        .status()
        .context("running makensis")?;
    if !status.success() {
        bail!("makensis failed; see the output above");
    }

    Ok(Some(installer_path))
}

/// Writes an NSIS script as UTF-8 with a BOM, which is what `Unicode true`
/// expects.
fn write_utf8_bom(path: &Path, contents: &str) -> Result<()> {
    let mut bytes = Vec::with_capacity(contents.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(contents.as_bytes());
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// Files copied into `bin/` when an FFmpeg install is bundled.
///
/// `ffmpeg.exe` imports `avdevice-63.dll` at load time even though ClipForge
/// never opens a capture device, so it has to ship: without it Windows fails
/// with "avdevice-63.dll not found" before `ffmpeg` runs at all. This was only
/// invisible on a dev machine because `C:\tools\ffmpeg\bin` was on `PATH`.
///
/// `ffplay.exe` is the one file that is genuinely unused; skipping it saves
/// ~17 MB.
const BUNDLED_FFMPEG_FILES: &[&str] = &[
    "ffmpeg.exe",
    "ffprobe.exe",
    "avcodec-*.dll",
    "avdevice-*.dll",
    "avfilter-*.dll",
    "avformat-*.dll",
    "avutil-*.dll",
    "swresample-*.dll",
    "swscale-*.dll",
];

/// Runs the bundled `ffmpeg.exe` with `PATH` cleared except for the bundle's
/// own directory.
///
/// Without this the bundle was only ever tested on a machine that also had a
/// system-wide FFmpeg on `PATH`, so a missing DLL went unnoticed: Windows found
/// it in `C:\tools\ffmpeg\bin`. Clearing `PATH` forces the process to resolve
/// every import from `bin/`, which is what a fresh machine sees.
fn verify_bundle(bin_dir: &Path) -> Result<()> {
    for exe in ["ffmpeg.exe", "ffprobe.exe"] {
        let output = Command::new(bin_dir.join(exe))
            .arg("-version")
            .env_clear()
            // Keep the few variables Windows itself needs to start a process.
            .env(
                "SystemRoot",
                std::env::var("SystemRoot").unwrap_or_default(),
            )
            .env("PATH", bin_dir)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("running bundled {exe}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "bundled {exe} failed to start with an isolated PATH — a required DLL is \
                 probably missing from {}.\n{stderr}",
                bin_dir.display()
            );
        }
    }

    println!("verified the bundled FFmpeg starts without a system install");
    Ok(())
}

/// Finds an FFmpeg build to bundle.
///
/// Tried in order: `CLIPFORGE_FFMPEG_DIR`, a vendored `vendor/ffmpeg/bin`, then
/// the location a developer on this machine is likely to have it in.
fn find_ffmpeg_install() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }

    std::env::var_os("CLIPFORGE_FFMPEG_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            repo_root().join("vendor").join("ffmpeg").join("bin"),
            PathBuf::from(r"C:\tools\ffmpeg\bin"),
        ])
        .find(|dir| dir.join("ffmpeg.exe").is_file() && dir.join("ffprobe.exe").is_file())
}

/// Copies an FFmpeg build into `<out>/bin`.
fn bundle_ffmpeg(source: &Path, out_dir: &Path) -> Result<PathBuf> {
    let dest = out_dir.join("bin");
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;

    let mut copied = 0usize;
    for pattern in BUNDLED_FFMPEG_FILES {
        let entries =
            std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if !glob_match(pattern, &name) {
                continue;
            }
            std::fs::copy(&path, dest.join(&name))
                .with_context(|| format!("copying {}", path.display()))?;
            copied += 1;
        }
    }

    if copied == 0 {
        bail!(
            "found {} but none of the expected files matched",
            source.display()
        );
    }

    println!("bundled FFmpeg from {} ({copied} files)", source.display());
    Ok(source.to_path_buf())
}

/// Minimal `*` glob for the DLL names; full pattern matching is overkill here.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
        None => pattern == name,
    }
}

/// Looks for `makensis` on `PATH`, then in the two standard NSIS folders.
fn find_makensis() -> Option<PathBuf> {
    if Command::new("makensis")
        .arg("/VERSION")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Some(PathBuf::from("makensis"));
    }

    let candidates = [
        r"C:\Program Files (x86)\NSIS\makensis.exe",
        r"C:\Program Files\NSIS\makensis.exe",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn quickstart(version: &str, bundled_ffmpeg: Option<PathBuf>) -> String {
    let edition = if bundled_ffmpeg.is_some() {
        "This is the FULL build: FFmpeg is included in `bin`."
    } else {
        "This is the SLIM build: FFmpeg is NOT included."
    };
    let ffmpeg = match bundled_ffmpeg {
        Some(_) => "1. FFmpeg is already included in the `bin` folder, so no separate
   install is needed. See THIRD_PARTY.md for its licence and source."
            .to_owned(),
        None => "1. FFmpeg is required but was NOT included in this build.

   Either:
     a) install FFmpeg so that `ffmpeg` and `ffprobe` are on PATH, or
     b) put ffmpeg.exe and ffprobe.exe (plus their DLLs) into a folder named
        `bin` next to clipforge.exe.

   ClipForge needs FFmpeg 6.0 or newer, with libx264 for precise export."
            .to_owned(),
    };

    format!(
        "ClipForge {version}
=================

This is a portable build. Nothing is installed; delete the folder to remove it.

{edition}

{ffmpeg}

2. Double-click clipforge.exe, or drag a video file onto it.

3. Settings live in %APPDATA%\\ClipForge\\ClipForge\\config\\preferences.json

Known limitations
-----------------
* Lossless export can only cut on keyframes; the dialog shows the real cut
  points before you start.
* Lossless concatenation requires identical codec parameters in every clip.
  Use precise export when they differ.
* Audio plays during preview; there is no audio-track editing yet.
"
    )
}

fn workspace_version(root: &Path) -> Result<String> {
    let cargo =
        std::fs::read_to_string(root.join("Cargo.toml")).context("reading workspace Cargo.toml")?;

    let mut in_workspace_package = false;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
            continue;
        }
        if in_workspace_package {
            if let Some(rest) = trimmed.strip_prefix("version") {
                if let Some(value) = rest.trim_start().strip_prefix('=') {
                    return Ok(value.trim().trim_matches('"').to_owned());
                }
            }
        }
    }
    bail!("could not find [workspace.package] version in Cargo.toml")
}

fn host_target() -> Result<String> {
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .context("running rustc to detect the host target")?;

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(target) = line.strip_prefix("host: ") {
            return Ok(target.trim().to_owned());
        }
    }
    bail!("rustc did not report a host target")
}

fn list_dir(dir: &Path, indent: &str) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            println!("{indent}{name}/");
            list_dir(&path, &format!("{indent}  "))?;
        } else {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("{indent}{name}  ({})", human_size(size));
        }
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn quickstart_mentions_ffmpeg_and_limitations() {
        let text = quickstart("9.9.9", None);
        assert!(text.contains("FFmpeg"));
        assert!(text.contains("9.9.9"));
        assert!(text.contains("keyframes"));
        assert!(text.contains("bin"));
        assert!(text.contains("NOT included"));
    }

    #[test]
    fn quickstart_says_when_ffmpeg_is_bundled() {
        let text = quickstart("9.9.9", Some(PathBuf::from(r"C:\tools\ffmpeg\bin")));
        assert!(text.contains("already included"));
        assert!(text.contains("FULL build"));
        assert!(!text.contains("was NOT included"));

        let slim = quickstart("9.9.9", None);
        assert!(slim.contains("SLIM build"));
        assert!(slim.contains("NOT included"));
    }

    #[test]
    fn variants_only_differ_by_suffix() {
        assert_eq!(Variant::Full.suffix(), "");
        assert_eq!(Variant::Slim.suffix(), "-slim");
        assert_ne!(Variant::Full.label(), Variant::Slim.label());
    }

    #[test]
    fn glob_matches_star_patterns() {
        assert!(glob_match("ffmpeg.exe", "ffmpeg.exe"));
        assert!(!glob_match("ffmpeg.exe", "ffprobe.exe"));
        assert!(glob_match("avcodec-*.dll", "avcodec-63.dll"));
        assert!(!glob_match("avcodec-*.dll", "avformat-63.dll"));
        assert!(!glob_match("avcodec-*.dll", "avcodec-63.dll.bak"));
    }
}
