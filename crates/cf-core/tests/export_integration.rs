//! Integration tests for the export pipeline.
//!
//! These run real FFmpeg and are ignored by default so `cargo test` stays fast
//! and works on machines without FFmpeg. Run with:
//!
//! ```text
//! cargo test --test export_integration -- --ignored --nocapture
//! ```

use cf_core::domain::{Clip, ClipId, MediaSource, Project, SourceId};
use cf_core::media::export::{build_lossless_command, plan_lossless, temp_path};
use cf_core::media::{discover, keyframes, probe};
use cf_core::time::Time;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
}

fn require_ffmpeg() -> cf_core::media::FfmpegInstall {
    match discover(None) {
        Ok((install, _)) => install,
        Err(err) => panic!("FFmpeg is required for integration tests: {err}"),
    }
}

/// Runs a command and returns `(success, stderr)`.
fn run(mut cmd: Command) -> (bool, String) {
    let output = cmd
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("running ffmpeg");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn probe_codecs(install: &cf_core::media::FfmpegInstall, path: &Path) -> Vec<String> {
    let out = Command::new(&install.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn probe_duration(install: &cf_core::media::FfmpegInstall, path: &Path) -> f64 {
    let out = Command::new(&install.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

#[test]
#[ignore = "requires FFmpeg and writes files"]
fn lossless_export_preserves_codecs() {
    let install = require_ffmpeg();
    let source = fixture("sample_h264.mp4");
    assert!(source.exists(), "run the fixture generator first");

    let info = probe::probe(&install, &source).expect("probe");
    let index = keyframes::probe(&install, &source).expect("keyframes");
    let audio = info.has_audio();

    let clip = Clip {
        id: ClipId(1),
        source_id: SourceId(1),
        source_in: Time::from_secs(1.0),
        source_out: Time::from_secs(3.0),
        timeline_start: Time::ZERO,
        group: None,
        name: "sample".into(),
    };
    let media = MediaSource::new(SourceId(1), info, index);
    let output = std::env::temp_dir().join("clipforge-export-codecs.mp4");
    let plan = plan_lossless(&clip, &media, &media.keyframes, &output);
    assert!(plan.is_runnable(), "plan blocked: {:?}", plan.blocker);

    let temp = temp_path(&output);
    let cmd = build_lossless_command(&install, &source, &temp, &plan, audio, &output);
    let (ok, stderr) = run(cmd);
    assert!(ok, "ffmpeg failed: {stderr}");

    std::fs::rename(&temp, &output).expect("rename");

    // Codecs must be identical to the source: that is the whole promise.
    assert_eq!(
        probe_codecs(&install, &source),
        probe_codecs(&install, &output),
        "stream copy changed the codec set"
    );

    let duration = probe_duration(&install, &output);
    assert!(
        (duration - plan.real_duration.as_secs_f64()).abs() < 0.3,
        "duration {duration} does not match the plan {}",
        plan.real_duration.as_secs_f64()
    );

    let _ = std::fs::remove_file(&output);
}

#[test]
#[ignore = "requires FFmpeg and writes files"]
fn lossless_export_of_a_video_without_audio_works() {
    let install = require_ffmpeg();
    let source = fixture("sample_noaudio.webm");
    if !source.exists() {
        eprintln!("skipping: {} missing", source.display());
        return;
    }

    let info = probe::probe(&install, &source).expect("probe");
    assert!(!info.has_audio());
    let index = keyframes::probe(&install, &source).expect("keyframes");

    let clip = Clip {
        id: ClipId(1),
        source_id: SourceId(1),
        source_in: Time::ZERO,
        source_out: Time::from_secs(2.0),
        timeline_start: Time::ZERO,
        group: None,
        name: "silent".into(),
    };
    let media = MediaSource::new(SourceId(1), info, index);
    let output = std::env::temp_dir().join("clipforge-export-silent.webm");
    let plan = plan_lossless(&clip, &media, &media.keyframes, &output);
    assert!(plan.is_runnable());

    let temp = temp_path(&output);
    let cmd = build_lossless_command(&install, &source, &temp, &plan, false, &output);
    let (ok, stderr) = run(cmd);
    assert!(ok, "ffmpeg failed: {stderr}");

    std::fs::rename(&temp, &output).expect("rename");
    let codecs = probe_codecs(&install, &output);
    assert!(codecs.iter().any(|line| line.contains("vp9")));
    let _ = std::fs::remove_file(&output);
}

#[test]
#[ignore = "requires FFmpeg and writes files"]
fn cancelled_export_leaves_no_part_file() {
    let install = require_ffmpeg();
    let source = fixture("sample_h264.mp4");
    if !source.exists() {
        eprintln!("skipping: {} missing", source.display());
        return;
    }

    let info = probe::probe(&install, &source).expect("probe");
    let index = keyframes::probe(&install, &source).expect("keyframes");
    let audio = info.has_audio();

    let clip = Clip {
        id: ClipId(1),
        source_id: SourceId(1),
        source_in: Time::ZERO,
        source_out: Time::from_secs(3.0),
        timeline_start: Time::ZERO,
        group: None,
        name: "sample".into(),
    };
    let media = MediaSource::new(SourceId(1), info, index);
    let output = std::env::temp_dir().join("clipforge-export-cancel.mp4");
    let plan = plan_lossless(&clip, &media, &media.keyframes, &output);

    let temp = temp_path(&output);
    let mut child = build_lossless_command(&install, &source, &temp, &plan, audio, &output)
        .spawn()
        .expect("spawn");

    // Kill almost immediately; the test is about cleanup, not throughput.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let _ = child.kill();
    let _ = child.wait();

    cf_core::media::export::cleanup_temp(&temp);
    assert!(!temp.exists(), "temp file survived cancellation");
    assert!(
        !output.exists() || std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0) == 0,
        "a cancelled export must not leave a finished-looking output"
    );
}

#[test]
fn plan_helper_compiles_and_blocks_on_missing_source() {
    // Cheap guard that keeps the helper honest without running FFmpeg.
    let project = Project::empty();
    let clip = Clip {
        id: ClipId(1),
        source_id: SourceId(99),
        source_in: Time::ZERO,
        source_out: Time::from_secs(1.0),
        timeline_start: Time::ZERO,
        group: None,
        name: "ghost".into(),
    };
    assert!(cf_core::media::export::resolve(&project, &clip).is_err());
}
