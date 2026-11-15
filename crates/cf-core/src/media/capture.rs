//! Frame capture: write the playhead frame to an image file.
//!
//! Two responsibilities, both UI-free so they can be tested without FFmpeg or
//! a window:
//!
//! * **Naming** — `<source stem><suffix>.<ext>`, e.g. `Main Edit.mp4` captured
//!   with `-poster` becomes `Main Edit-poster.png`. Collisions get `-2`, `-3`,
//!   … so a second capture never overwrites the first.
//! * **Decoding** — one FFmpeg call at the source's native resolution, with
//!   rotation baked in the same way the preview does it.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::probe::MediaInfo;
use crate::time::Time;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Image format for a capture, chosen by the file extension.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// Picks the format from a path's extension; defaults to PNG.
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("jpg" | "jpeg") => Self::Jpeg,
            _ => Self::Png,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }

    /// Encoder arguments after the scale filter.
    fn encoder_args(self) -> &'static [&'static str] {
        match self {
            Self::Png => &["-c:v", "png"],
            // q=2 is the codec's "visually lossless" setting; 4:4:4 keeps
            // fine text and edges from smearing, which matters for a poster
            // frame pulled out of a screen recording.
            Self::Jpeg => &["-c:v", "mjpeg", "-q:v", "2", "-pix_fmt", "yuvj444p"],
        }
    }
}

/// Default suffix when the preference has never been set.
pub const DEFAULT_SUFFIX: &str = "-poster";

/// Builds the output path for a capture.
///
/// `suffix` is appended verbatim, so `-poster` yields `Main Edit-poster.png`.
/// The result lands next to the source unless `suffix` starts with a separator
/// (users may want a subfolder), which is respected as-is.
pub fn capture_path(
    source: &Path,
    suffix: &str,
    format: ImageFormat,
    taken: &[PathBuf],
) -> PathBuf {
    let dir = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_owned());

    // The suffix is user text: strip anything a filesystem would reject, but
    // keep leading dots/dashes because `-poster` and `.poster` are both valid
    // and common.
    let cleaned: String = suffix
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect();

    let base = format!("{stem}{cleaned}");
    let mut candidate = dir.join(format!("{base}.{}", format.extension()));

    // `taken` covers captures made in this session; the filesystem check
    // covers everything already on disk.
    let mut counter = 2;
    while taken.contains(&candidate) || candidate.exists() {
        candidate = dir.join(format!("{base}-{counter}.{}", format.extension()));
        counter += 1;
    }
    candidate
}

/// Where the image will go, without writing it.
///
/// `-f image2` needs the codec to imply a muxer, which it does for png/mjpeg.
fn filter(info: &MediaInfo) -> String {
    // Rotation is baked into the pixels exactly like the preview does it, so a
    // portrait phone video yields a portrait still.
    let transpose = match info.video.rotation.rem_euclid(360) {
        90 => Some("transpose=clock"),
        180 => Some("transpose=clock,transpose=clock"),
        270 => Some("transpose=cclock"),
        _ => None,
    };
    transpose.unwrap_or("null").to_owned()
}

/// Writes one frame at `at` to `output` at the source's native resolution.
///
/// Runs FFmpeg synchronously: a single frame is ~100-300 ms even at 4K, and
/// the caller already runs this off the UI thread.
pub fn capture_frame(
    install: &FfmpegInstall,
    source: &Path,
    info: &MediaInfo,
    at: Time,
    output: &Path,
) -> Result<PathBuf, CoreError> {
    let format = ImageFormat::from_path(output);
    let seconds = at.as_secs_f64().max(0.0);

    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-ss"])
        .arg(format!("{seconds:.6}"))
        .arg("-i")
        .arg(source)
        .args(["-frames:v", "1", "-an", "-sn", "-dn"]);

    // Only pay for a filter when the source is rotated; `null` is a no-op
    // muxer-wise but still forces a filter graph, so skip it entirely.
    if info.video.rotation.rem_euclid(360) != 0 {
        cmd.args(["-vf", &filter(info)]);
    }

    cmd.args(["-y"])
        .args(format.encoder_args())
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output_result = cmd.output().map_err(|source_err| CoreError::FfmpegSpawn {
        path: install.ffmpeg.clone(),
        source: source_err,
    })?;

    if !output_result.status.success() {
        return Err(CoreError::FfmpegFailed {
            program: install.ffmpeg.display().to_string(),
            code: output_result.status.code(),
            stderr: String::from_utf8_lossy(&output_result.stderr)
                .trim()
                .to_owned(),
        });
    }

    Ok(output.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> ImageFormat {
        ImageFormat::Png
    }

    #[test]
    fn appends_the_suffix_before_the_extension() {
        let path = capture_path(Path::new("C:/videos/Main Edit.mp4"), "-poster", png(), &[]);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "Main Edit-poster.png"
        );
    }

    #[test]
    fn keeps_the_source_directory() {
        let path = capture_path(Path::new("C:/videos/Main Edit.mp4"), "-poster", png(), &[]);
        assert_eq!(path.parent(), Some(Path::new("C:/videos")));
    }

    #[test]
    fn empty_suffix_reuses_the_source_name() {
        let path = capture_path(Path::new("C:/videos/clip.mkv"), "", png(), &[]);
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "clip.png");
    }

    #[test]
    fn jpeg_uses_the_jpg_extension() {
        let path = capture_path(
            Path::new("C:/videos/clip.mov"),
            "-poster",
            ImageFormat::Jpeg,
            &[],
        );
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "clip-poster.jpg"
        );
    }

    #[test]
    fn a_taken_path_gets_a_counter() {
        let taken = vec![PathBuf::from("C:/videos/clip-poster.png")];
        let path = capture_path(Path::new("C:/videos/clip.mp4"), "-poster", png(), &taken);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "clip-poster-2.png"
        );
    }

    #[test]
    fn many_taken_paths_keep_counting_up() {
        let taken: Vec<PathBuf> = (0..3)
            .map(|i| {
                let name = if i == 0 {
                    "clip-poster.png".to_owned()
                } else {
                    format!("clip-poster-{}.png", i + 1)
                };
                PathBuf::from("C:/videos").join(name)
            })
            .collect();
        let path = capture_path(Path::new("C:/videos/clip.mp4"), "-poster", png(), &taken);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "clip-poster-4.png"
        );
    }

    #[test]
    fn strips_characters_filesystems_reject() {
        let path = capture_path(Path::new("C:/v/clip.mp4"), "-a<b>:c|d?e*f", png(), &[]);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "clip-abcdef.png"
        );
    }

    #[test]
    fn format_follows_the_extension() {
        assert_eq!(ImageFormat::from_path(Path::new("a.PNG")), ImageFormat::Png);
        assert_eq!(
            ImageFormat::from_path(Path::new("a.jpeg")),
            ImageFormat::Jpeg
        );
        // Anything unknown falls back to the lossless default.
        assert_eq!(ImageFormat::from_path(Path::new("a.bmp")), ImageFormat::Png);
    }
}
