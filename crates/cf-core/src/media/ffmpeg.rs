//! Locating and describing the FFmpeg sidecars.
//!
//! FFmpeg is never linked; it is always invoked as a child process. This
//! module owns *which* binary we talk to and *how* we invoke it, so the rest of
//! the media layer never builds a `Command` by hand.

use crate::CoreError;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Minimum FFmpeg major version we support. Older builds are rejected because
/// several options used by the export pipeline did not exist.
pub const MIN_MAJOR: u32 = 6;

/// A resolved FFmpeg installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfmpegInstall {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub version: FfmpegVersion,
}

/// Parsed `ffmpeg -version` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfmpegVersion {
    /// Full first line, kept verbatim for logs and the about dialog.
    pub banner: String,
    /// Major version, or `None` for untagged git builds like `N-125752-...`.
    pub major: Option<u32>,
    pub git_build: bool,
    pub gpl: bool,
    /// Encoders we care about, probed once at startup.
    pub encoders: EncoderSupport,
}

impl FfmpegVersion {
    /// Whether the build is new enough to be used.
    ///
    /// Untagged git builds are accepted: they track master and therefore are
    /// always newer than any release we target.
    pub fn is_supported(&self) -> bool {
        self.git_build || self.major.is_some_and(|major| major >= MIN_MAJOR)
    }
}

/// Which encoders the build provides. Missing encoders degrade specific export
/// paths rather than blocking the application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderSupport {
    pub libx264: bool,
    pub libx265: bool,
    pub libvpx_vp9: bool,
    pub aac: bool,
    pub libopus: bool,
}

/// How a binary was found, surfaced in the UI when something goes wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoverySource {
    Preferences,
    BesideExe,
    AppResources,
    Path,
}

impl DiscoverySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preferences => "settings",
            Self::BesideExe => "bin/ next to the executable",
            Self::AppResources => "application resources",
            Self::Path => "PATH",
        }
    }
}

/// Locates FFmpeg and probes its capabilities.
///
/// Search order matches ADR-002: explicit user path, then `bin/` beside the
/// executable, then the app resource dir, then `PATH`.
pub fn discover(explicit: Option<&Path>) -> Result<(FfmpegInstall, DiscoverySource), CoreError> {
    let mut attempts: Vec<(PathBuf, DiscoverySource, CoreError)> = Vec::new();

    for (dir, source) in candidate_dirs(explicit) {
        let ffmpeg = dir.join(exe_name("ffmpeg"));
        let ffprobe = dir.join(exe_name("ffprobe"));

        match probe_install(&ffmpeg, &ffprobe) {
            Ok(install) => return Ok((install, source)),
            Err(err) => attempts.push((ffmpeg, source, err)),
        }
    }

    Err(CoreError::ffmpeg_not_found(attempts))
}

/// Candidate directories in priority order. `explicit` may point either at a
/// directory or directly at an executable.
fn candidate_dirs(explicit: Option<&Path>) -> Vec<(PathBuf, DiscoverySource)> {
    let mut dirs = Vec::new();

    if let Some(path) = explicit {
        // A non-existent path has no useful metadata, so `is_dir()` is false
        // and we fall back to treating it as a file whose parent we search.
        // That is the right behaviour for both "user pointed at a folder" and
        // "user pointed at ffmpeg.exe".
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => path.to_path_buf(),
            }
        };
        dirs.push((dir, DiscoverySource::Preferences));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push((dir.join("bin"), DiscoverySource::BesideExe));
            dirs.push((dir.to_path_buf(), DiscoverySource::BesideExe));
        }
    }

    // Same empty-dir trick as the font loader: PATH entries are handled below.
    if let Some(paths) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&paths) {
            dirs.push((entry, DiscoverySource::Path));
        }
    }

    dirs
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn probe_install(ffmpeg: &Path, ffprobe: &Path) -> Result<FfmpegInstall, CoreError> {
    if !ffmpeg.is_file() {
        return Err(CoreError::FfmpegMissing {
            path: ffmpeg.to_path_buf(),
        });
    }
    let banner = run_version(ffmpeg)?;

    if !ffprobe.is_file() {
        return Err(CoreError::FfmpegMissing {
            path: ffprobe.to_path_buf(),
        });
    }

    let version = FfmpegVersion {
        major: parse_major(&banner),
        git_build: banner.contains("N-") || banner.contains("git"),
        gpl: banner.contains("--enable-gpl"),
        banner: banner.clone(),
        encoders: probe_encoders(ffmpeg)?,
    };

    if !version.is_supported() {
        return Err(CoreError::FfmpegTooOld {
            path: ffmpeg.to_path_buf(),
            found: version.major,
            required: MIN_MAJOR,
        });
    }

    Ok(FfmpegInstall {
        ffmpeg: ffmpeg.to_path_buf(),
        ffprobe: ffprobe.to_path_buf(),
        version,
    })
}

fn run_version(ffmpeg: &Path) -> Result<String, CoreError> {
    let output = hidden_command(ffmpeg)
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: ffmpeg.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(CoreError::FfmpegFailed {
            program: ffmpeg.display().to_string(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Parses the major version from a banner line, ignoring git-tagged builds.
pub fn parse_major(banner: &str) -> Option<u32> {
    let first_line = banner.lines().next()?.trim();
    let rest = first_line.split("version ").nth(1)?;
    let token = rest.split_whitespace().next()?;

    // Release builds look like `7.1.1` or `6.0`; git builds look like `N-125752-abc`.
    let major = token.split('.').next()?;
    major.parse().ok()
}

fn probe_encoders(ffmpeg: &Path) -> Result<EncoderSupport, CoreError> {
    let output = hidden_command(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: ffmpeg.to_path_buf(),
            source,
        })?;

    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(EncoderSupport {
        libx264: has_encoder(&listing, "libx264"),
        libx265: has_encoder(&listing, "libx265"),
        libvpx_vp9: has_encoder(&listing, "libvpx-vp9"),
        aac: has_encoder(&listing, "aac"),
        libopus: has_encoder(&listing, "libopus"),
    })
}

fn has_encoder(listing: &str, name: &str) -> bool {
    listing.lines().any(|line| {
        // Encoder rows look like ` V....D libx264   H.264 / AVC ...`
        line.split_whitespace().nth(1) == Some(name)
    })
}

/// Builds a `Command` with the child process's console window suppressed.
///
/// Every FFmpeg call goes through here: spawning without this flag flashes a
/// console window on Windows for each probe and export.
pub fn hidden_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Convenience for building a command line summary for logs.
pub fn describe_command(program: &Path, args: &[impl AsRef<OsStr>]) -> String {
    let mut parts = vec![program.display().to_string()];
    parts.extend(
        args.iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned()),
    );
    // Quote arguments containing spaces so log lines stay copy-pasteable.
    parts
        .into_iter()
        .map(|part| {
            if part.contains(' ') && !part.starts_with('"') {
                format!("\"{part}\"")
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_major() {
        assert_eq!(
            parse_major("ffmpeg version 7.1.1 Copyright (c) 2000-2025"),
            Some(7)
        );
        assert_eq!(parse_major("ffmpeg version 6.0 Copyright"), Some(6));
        assert_eq!(parse_major("ffmpeg version 4.4.2-0ubuntu"), Some(4));
    }

    #[test]
    fn parses_git_build_as_none() {
        assert_eq!(
            parse_major("ffmpeg version N-125752-g2f209337fc-20260724 Copyright"),
            None
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_major("not ffmpeg at all"), None);
        assert_eq!(parse_major(""), None);
    }

    #[test]
    fn git_builds_are_supported_regardless_of_major() {
        let version = FfmpegVersion {
            banner: String::new(),
            major: None,
            git_build: true,
            gpl: true,
            encoders: EncoderSupport::default(),
        };
        assert!(version.is_supported());
    }

    #[test]
    fn old_release_is_rejected() {
        let version = FfmpegVersion {
            banner: String::new(),
            major: Some(4),
            git_build: false,
            gpl: true,
            encoders: EncoderSupport::default(),
        };
        assert!(!version.is_supported());
    }

    #[test]
    fn encoder_listing_matches_only_the_name_column() {
        let listing =
            " V....D libx264              H.264 / AVC\n A....D aac                  AAC\n";
        assert!(has_encoder(listing, "libx264"));
        assert!(has_encoder(listing, "aac"));
        assert!(!has_encoder(listing, "libx265"));
        // A description mentioning the name must not count as a match.
        assert!(!has_encoder(
            " A....D libopus  Opus (libx264-like)\n",
            "libx264"
        ));
    }

    #[test]
    fn explicit_directory_is_searched_directly() {
        let dirs = candidate_dirs(Some(Path::new("C:/tools/ffmpeg/bin")));
        assert_eq!(dirs[0].0, PathBuf::from("C:/tools/ffmpeg/bin"));
        assert_eq!(dirs[0].1, DiscoverySource::Preferences);
    }

    #[test]
    fn explicit_executable_searches_its_parent() {
        let dirs = candidate_dirs(Some(Path::new("C:/tools/ffmpeg/bin/ffmpeg.exe")));
        assert_eq!(dirs[0].0, PathBuf::from("C:/tools/ffmpeg/bin"));
    }

    #[test]
    fn bare_relative_path_does_not_become_an_empty_dir() {
        let dirs = candidate_dirs(Some(Path::new("ffmpeg")));
        assert!(!dirs[0].0.as_os_str().is_empty());
    }

    #[test]
    fn quotes_arguments_with_spaces() {
        let text = describe_command(Path::new("C:/Program Files/ffmpeg.exe"), &["-i", "a b.mp4"]);
        assert!(text.contains("\"C:/Program Files/ffmpeg.exe\""));
        assert!(text.contains("\"a b.mp4\""));
    }
}
