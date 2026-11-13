//! Lossless export: stream copy.
//!
//! "Lossless" means `-c copy`: no decode, no encode. The output keeps the
//! source's codecs and container. The catch every NLE shares is that a copy
//! can only start on a keyframe, so the real cut points may drift from what the
//! user picked — this module's job is to compute that drift *before* running
//! anything, so the UI can show it.
//!
//! Single-clip stream copy. Multi-segment concatenation lives in
//! `cf-app/src/export.rs`, which drives this module once per segment.

use super::ExportBlocker;
use crate::domain::{Clip, MediaSource, Project};
use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::keyframes::KeyframeIndex;
use crate::time::Time;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the user wants to export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LosslessRequest {
    /// Clip being exported.
    pub clip: Clip,
    /// Absolute path to write. Must not exist yet, or the caller must have
    /// confirmed overwriting.
    pub output: PathBuf,
    /// Source file.
    pub source_path: PathBuf,
}

/// The result of checking a request before running FFmpeg.
#[derive(Clone, Debug, PartialEq)]
pub struct LosslessPlan {
    /// Cut points after snapping to keyframes.
    pub real_start: Time,
    pub real_end: Time,
    /// How far the real cut moved from what the user asked for.
    pub start_drift: Time,
    pub end_drift: Time,
    /// Duration that will actually be written.
    pub real_duration: Time,
    /// Why the plan cannot run, if it cannot.
    pub blocker: Option<ExportBlocker>,
}

impl LosslessPlan {
    pub fn is_runnable(&self) -> bool {
        self.blocker.is_none()
    }
}

/// Computes the real cut points for a clip.
///
/// In-point snaps *back* to the previous keyframe and the out-point snaps
/// *forward* to the next one: a stream copy cannot start mid-GOP, and stopping
/// short would silently drop the tail the user asked for.
pub fn plan_lossless(
    clip: &Clip,
    source: &MediaSource,
    keyframes: &KeyframeIndex,
    output: &Path,
) -> LosslessPlan {
    let requested_start = clip.source_in;
    let requested_end = clip.source_out;

    let (real_start, real_end) = if keyframes.is_degraded() || keyframes.is_empty() {
        (requested_start, requested_end)
    } else if let Some((start, end)) = keyframes.snap_range(requested_start, requested_end) {
        (start, end)
    } else {
        (requested_start, requested_end)
    };

    let real_duration = real_end.saturating_sub(real_start);

    let blocker = if keyframes.is_degraded() || keyframes.is_empty() {
        Some(ExportBlocker::KeyframesUnavailable)
    } else if real_duration <= Time::ZERO {
        Some(ExportBlocker::EmptyRange)
    } else if same_file(output, &source.info.path) {
        Some(ExportBlocker::OutputIsInput)
    } else {
        None
    };

    LosslessPlan {
        real_start,
        real_end,
        start_drift: real_start.saturating_sub(requested_start),
        end_drift: real_end.saturating_sub(requested_end),
        real_duration,
        blocker,
    }
}

/// True when two paths point at the same file, tolerating case and separators.
pub fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// Container name for an output path, used to pass `-f` explicitly.
///
/// FFmpeg infers the muxer from the extension, but our temp file is named
/// `.clip.mp4.part` so a cancelled export never looks finished — and FFmpeg
/// cannot see through the second extension. Passing `-f` keeps the `.part`
/// trick working.
pub fn container_for(output: &Path) -> Option<&'static str> {
    let ext = output
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "mp4" | "m4v" => Some("mp4"),
        "mov" => Some("mov"),
        "mkv" => Some("matroska"),
        "webm" => Some("webm"),
        "ts" => Some("mpegts"),
        _ => None,
    }
}

/// Builds the `ffmpeg` command line for a lossless single-clip export.
///
/// Deliberately takes the *file* arguments as temp + final: the caller writes
/// to a `.part` file and renames on success, so a cancelled export never
/// leaves a plausible-looking output behind.
pub fn build_lossless_command(
    install: &FfmpegInstall,
    source: &Path,
    temp_output: &Path,
    plan: &LosslessPlan,
    has_audio: bool,
    output_for_container: &Path,
) -> Command {
    let mut cmd = hidden_command(&install.ffmpeg);

    cmd.args(["-hide_banner", "-nostdin", "-y"])
        // Seek before -i so FFmpeg jumps instead of decoding; with -c copy it
        // lands on the keyframe at or before the timestamp.
        .arg("-ss")
        .arg(format!("{:.6}", plan.real_start.as_secs_f64()))
        .arg("-i")
        .arg(source)
        .arg("-t")
        .arg(format!("{:.6}", plan.real_duration.as_secs_f64()))
        .args(["-map", "0:v:0"]);

    if has_audio {
        // `?` makes a missing audio track non-fatal.
        cmd.args(["-map", "0:a:0?"]);
    }

    cmd.args([
        "-c",
        "copy",
        // Copied timestamps can start negative; zero them so players do not
        // show a leading gap.
        "-avoid_negative_ts",
        "make_zero",
        // Keep the mov/mp4 edit list from confusing duration reporting.
        "-movflags",
        "+faststart",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);

    // The temp file's double extension hides the container from ffmpeg.
    if let Some(container) = container_for(output_for_container) {
        cmd.args(["-f", container]);
    }

    cmd.arg(temp_output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    cmd
}

/// Validates that the output path is usable before starting a long job.
pub fn validate_output(output: &Path, project: &Project) -> Result<(), ExportBlocker> {
    if output.as_os_str().is_empty() {
        return Err(ExportBlocker::EmptyRange);
    }
    for source in &project.sources {
        if same_file(output, &source.info.path) {
            return Err(ExportBlocker::OutputIsInput);
        }
    }
    Ok(())
}

/// Replaces `output` with `temp`, tolerating Windows' refusal to rename onto
/// an existing file.
pub fn finalize_output(temp: &Path, output: &Path) -> Result<(), CoreError> {
    #[cfg(windows)]
    if output.exists() {
        std::fs::remove_file(output).map_err(|source| CoreError::io(output, source))?;
    }
    std::fs::rename(temp, output).map_err(|source| CoreError::io(output, source))
}

/// Resolves the clip/source/keyframes a request needs.
#[allow(clippy::needless_lifetimes)]
pub fn resolve<'a>(
    project: &'a Project,
    clip: &Clip,
) -> Result<(&'a MediaSource, &'a KeyframeIndex), ExportBlocker> {
    let source = project
        .source(clip.source_id)
        .ok_or(ExportBlocker::MissingSource)?;
    Ok((source, source.keyframes.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceId;
    use crate::media::export::{format_drift, temp_path};
    use crate::media::probe;
    use std::ffi::OsStr;

    fn source(seconds: f64) -> MediaSource {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        let mut info = probe::parse(std::path::Path::new("C:/media/a.mp4"), &json).unwrap();
        info.duration = Time::from_secs(seconds);
        MediaSource::new(
            SourceId(1),
            info,
            KeyframeIndex::new(vec![
                Time::ZERO,
                Time::from_secs(2.0),
                Time::from_secs(4.0),
                Time::from_secs(6.0),
            ]),
        )
    }

    fn clip(source_in: f64, source_out: f64) -> Clip {
        Clip {
            id: crate::domain::ClipId(1),
            source_id: SourceId(1),
            source_in: Time::from_secs(source_in),
            source_out: Time::from_secs(source_out),
            timeline_start: Time::ZERO,
            group: None,
            name: "a.mp4".into(),
        }
    }

    fn install() -> FfmpegInstall {
        FfmpegInstall {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            version: crate::media::FfmpegVersion {
                banner: String::new(),
                major: Some(7),
                git_build: false,
                gpl: true,
                encoders: Default::default(),
            },
        }
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn snaps_outward_to_keyframes() {
        let source = source(10.0);
        let clip = clip(2.5, 4.5);
        let plan = plan_lossless(&clip, &source, &source.keyframes, Path::new("C:/out.mp4"));

        assert_eq!(plan.real_start, Time::from_secs(2.0), "start snaps back");
        assert_eq!(plan.real_end, Time::from_secs(6.0), "end snaps forward");
        assert_eq!(plan.start_drift, Time::from_secs(-0.5));
        assert_eq!(plan.end_drift, Time::from_secs(1.5));
        assert_eq!(plan.real_duration, Time::from_secs(4.0));
        assert!(plan.is_runnable());
    }

    #[test]
    fn exact_keyframe_cut_has_no_drift() {
        let source = source(10.0);
        let clip = clip(2.0, 6.0);
        let plan = plan_lossless(&clip, &source, &source.keyframes, Path::new("C:/out.mp4"));
        assert_eq!(plan.start_drift, Time::ZERO);
        assert_eq!(plan.end_drift, Time::ZERO);
    }

    #[test]
    fn degraded_keyframes_block_the_export() {
        let mut source = source(10.0);
        source.keyframes = std::sync::Arc::new(KeyframeIndex::degraded());
        let plan = plan_lossless(
            &clip(1.0, 3.0),
            &source,
            &source.keyframes,
            Path::new("C:/out.mp4"),
        );
        assert_eq!(plan.blocker, Some(ExportBlocker::KeyframesUnavailable));
        assert!(!plan.is_runnable());
    }

    #[test]
    fn exporting_onto_the_source_is_refused() {
        let source = source(10.0);
        let plan = plan_lossless(
            &clip(0.0, 2.0),
            &source,
            &source.keyframes,
            Path::new("C:/media/a.mp4"),
        );
        assert_eq!(plan.blocker, Some(ExportBlocker::OutputIsInput));
    }

    #[test]
    fn same_file_ignores_case_and_separators() {
        assert!(same_file(
            Path::new("C:/Media/A.mp4"),
            Path::new(r"C:\media\a.mp4")
        ));
        assert!(!same_file(
            Path::new("C:/media/a.mp4"),
            Path::new("C:/media/b.mp4")
        ));
    }

    #[test]
    fn command_uses_stream_copy_and_expected_flags() {
        let source = source(10.0);
        let plan = plan_lossless(
            &clip(2.5, 4.5),
            &source,
            &source.keyframes,
            Path::new("C:/out.mp4"),
        );
        let cmd = build_lossless_command(
            &install(),
            Path::new("C:/media/a.mp4"),
            Path::new("C:/out.part"),
            &plan,
            true,
            Path::new("C:/out.mp4"),
        );

        let args = args_of(&cmd);
        let joined = args.join(" ");

        assert!(joined.contains("-c copy"), "must not re-encode: {joined}");
        assert!(
            joined.contains("-ss 2.000000"),
            "seek uses the snapped start"
        );
        assert!(
            joined.contains("-t 4.000000"),
            "duration uses the snapped length"
        );
        assert!(joined.contains("-map 0:v:0"));
        assert!(joined.contains("-map 0:a:0?"));
        assert!(joined.contains("-avoid_negative_ts make_zero"));
        assert!(joined.contains("-progress pipe:1"));
        assert!(joined.contains("-nostats"));
        assert!(
            joined.contains("-f mp4"),
            "muxer must be explicit for .part"
        );

        // The output argument comes last.
        assert_eq!(args.last().map(String::as_str), Some("C:/out.part"));
    }

    #[test]
    fn command_omits_audio_mapping_when_there_is_no_audio() {
        let source = source(10.0);
        let plan = plan_lossless(
            &clip(0.0, 2.0),
            &source,
            &source.keyframes,
            Path::new("C:/out.mp4"),
        );
        let cmd = build_lossless_command(
            &install(),
            Path::new("C:/media/a.mp4"),
            Path::new("C:/out.part"),
            &plan,
            false,
            Path::new("C:/out.mp4"),
        );
        let joined = args_of(&cmd).join(" ");
        assert!(!joined.contains("0:a:0"), "no audio track to map");
    }

    #[test]
    fn container_is_derived_from_the_final_name() {
        assert_eq!(container_for(Path::new("a.mp4")), Some("mp4"));
        assert_eq!(container_for(Path::new("a.MOV")), Some("mov"));
        assert_eq!(container_for(Path::new("a.mkv")), Some("matroska"));
        assert_eq!(container_for(Path::new("a.webm")), Some("webm"));
        assert_eq!(container_for(Path::new("a.txt")), None);
    }

    #[test]
    fn temp_path_is_a_hidden_sibling() {
        let temp = temp_path(Path::new("C:/out/clip.mp4"));
        assert_eq!(
            temp.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some(".clip.mp4.part".to_owned())
        );
        assert_eq!(temp.parent(), Some(Path::new("C:/out")));
    }

    #[test]
    fn validate_output_rejects_an_input_path() {
        let mut project = Project::empty();
        let src = source(10.0);
        let path = src.info.path.clone();
        project.sources.push(src);

        assert_eq!(
            validate_output(&path, &project),
            Err(ExportBlocker::OutputIsInput)
        );
        assert_eq!(
            validate_output(Path::new("C:/other/out.mp4"), &project),
            Ok(())
        );
    }

    #[test]
    fn formats_drift_for_display() {
        assert_eq!(format_drift(Time::ZERO), "0");
        assert_eq!(format_drift(Time::from_secs(1.5)), "+1.500s");
        assert_eq!(format_drift(Time::from_secs(-0.5)), "-0.500s");
    }

    #[test]
    fn resolve_reports_a_missing_source() {
        let project = Project::empty();
        let err = resolve(&project, &clip(0.0, 1.0)).unwrap_err();
        assert_eq!(err, ExportBlocker::MissingSource);
    }

    #[test]
    fn source_path_helper_round_trips() {
        let source = source(10.0);
        assert_eq!(
            source.info.path.file_name().and_then(OsStr::to_str),
            Some("a.mp4")
        );
        let info: &crate::media::probe::MediaInfo = &source.info;
        assert_eq!(info.duration, Time::from_secs(10.0));
    }
}
