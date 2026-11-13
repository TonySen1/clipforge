//! Integration tests for multi-segment and precise export.
//!
//! Run with:
//!
//! ```text
//! cargo test -p cf-core --test export_p6_integration -- --ignored --nocapture
//! ```

use cf_core::domain::{ClipId, MediaSource, Project, SourceId};
use cf_core::media::export::{
    build_precise_command, plan_precise, temp_path, PreciseSettings, Segment,
};
use cf_core::media::{discover, keyframes, probe, FfmpegInstall};
use cf_core::time::Time;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
}

fn require_ffmpeg() -> FfmpegInstall {
    discover(None)
        .map(|(install, _)| install)
        .unwrap_or_else(|err| panic!("FFmpeg required: {err}"))
}

fn ffprobe_entries(install: &FfmpegInstall, path: &Path, entries: &str) -> String {
    let out = Command::new(&install.ffprobe)
        .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn register(project: &mut Project, path: &Path) -> (SourceId, bool) {
    let install = require_ffmpeg();
    let info = probe::probe(&install, path).expect("probe");
    let index = keyframes::probe(&install, path).expect("keyframes");
    let has_audio = info.has_audio();

    let id = project.alloc_source_id();
    let mut source = MediaSource::new(id, info, index);
    source.id = id;
    project.sources.push(source.clone());
    // Place it on the timeline end to end.
    project.add_source(source);
    (id, has_audio)
}

/// Two adjacent segments from the same file, exported via concat.
#[test]
#[ignore = "requires FFmpeg and writes files"]
fn lossless_concat_of_two_segments_works() {
    let install = require_ffmpeg();
    let source = fixture("sample_h264.mp4");
    assert!(source.exists());

    let info = probe::probe(&install, &source).expect("probe");
    let audio = info.has_audio();

    let segments = [
        Segment {
            source_id: SourceId(1),
            source_in: Time::ZERO,
            source_out: Time::from_secs(1.0),
            timeline_start: Time::ZERO,
            timeline_end: Time::from_secs(1.0),
        },
        Segment {
            source_id: SourceId(1),
            source_in: Time::from_secs(2.0),
            source_out: Time::from_secs(3.0),
            timeline_start: Time::from_secs(1.0),
            timeline_end: Time::from_secs(2.0),
        },
    ];

    let work = std::env::temp_dir().join("clipforge-p6-concat");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("work dir");

    let mut list = String::new();
    for (index, segment) in segments.iter().enumerate() {
        let path = work.join(format!("seg{index}.ts"));
        let status = Command::new(&install.ffmpeg)
            .args(["-v", "error", "-y", "-ss"])
            .arg(format!("{:.6}", segment.source_in.as_secs_f64()))
            .arg("-i")
            .arg(&source)
            .arg("-t")
            .arg(format!("{:.6}", segment.duration().as_secs_f64()))
            .args(["-map", "0:v:0"])
            .args(if audio {
                vec!["-map", "0:a:0?"]
            } else {
                vec![]
            })
            .args([
                "-c",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
                "-f",
                "mpegts",
            ])
            .arg(&path)
            .stderr(Stdio::null())
            .status()
            .expect("segment encode");
        assert!(status.success(), "segment {index} failed");
        list.push_str(&format!(
            "file '{}'\n",
            path.to_string_lossy().replace('\\', "/")
        ));
    }

    let list_path = work.join("list.txt");
    std::fs::write(&list_path, list).expect("list");

    let output = work.join("joined.mp4");
    let status = Command::new(&install.ffmpeg)
        .args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy", "-avoid_negative_ts", "make_zero", "-f", "mp4"])
        .arg(&output)
        .stderr(Stdio::piped())
        .status()
        .expect("concat");
    assert!(status.success());

    let codecs = ffprobe_entries(&install, &output, "stream=codec_name");
    assert!(codecs.contains("h264"), "codecs: {codecs}");

    let duration: f64 = ffprobe_entries(&install, &output, "format=duration")
        .parse()
        .unwrap_or(0.0);
    assert!(
        (duration - 2.0).abs() < 0.4,
        "concatenated duration {duration} should be about 2 s"
    );

    let _ = std::fs::remove_dir_all(&work);
}

/// A gap-filled precise export from a timeline with a hole.
#[test]
#[ignore = "requires FFmpeg and writes files"]
fn precise_export_fills_gaps() {
    let install = require_ffmpeg();
    let source = fixture("sample_h264.mp4");
    if !source.exists() {
        eprintln!("skipping: fixture missing");
        return;
    }

    let mut project = Project::empty();
    let (id, has_audio) = register(&mut project, &source);

    // Move the clip to 2 s so 0-2 s is a gap.
    let clip_id = project.video_track().unwrap().clips[0].id;
    project
        .move_clip(clip_id, Time::from_secs(2.0))
        .expect("move");

    let plan = plan_precise(
        &project,
        Time::ZERO,
        Time::from_secs(6.0),
        PreciseSettings::default(),
    );
    assert!(plan.is_runnable(), "plan blocked: {:?}", plan.blocker);
    assert!(plan.has_gaps(), "the leading 2 s should be a gap");

    let output = std::env::temp_dir().join("clipforge-p6-precise.mp4");
    let temp = temp_path(&output);

    let mut cmd = build_precise_command(
        &install,
        &plan,
        std::slice::from_ref(&source),
        &temp,
        &output,
        has_audio,
    )
    .expect("command");

    let status = cmd.stderr(Stdio::piped()).status().expect("run");
    assert!(status.success(), "precise export failed");

    std::fs::rename(&temp, &output).expect("rename");

    let duration: f64 = ffprobe_entries(&install, &output, "format=duration")
        .parse()
        .unwrap_or(0.0);
    assert!(
        (duration - 6.0).abs() < 0.6,
        "gap-filled duration {duration} should be about 6 s"
    );

    // The first two seconds must be black.
    let sample = Command::new(&install.ffmpeg)
        .args(["-v", "error", "-ss", "0.5", "-i"])
        .arg(&output)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=1:1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .expect("sample frame");
    let rgb = sample.stdout;
    assert_eq!(rgb.len(), 3, "expected one pixel");
    assert!(
        rgb.iter().all(|c| *c < 24),
        "gap should be black, got {rgb:?} for source {id:?}"
    );

    let _ = std::fs::remove_file(&output);
}

/// A precise export of two clips from *different* sources, which lossless
/// would refuse.
#[test]
#[ignore = "requires FFmpeg and writes files"]
fn precise_export_joins_incompatible_sources() {
    let install = require_ffmpeg();
    let first = fixture("sample_h264.mp4");
    let second = fixture("sample_noaudio.webm");
    if !first.exists() || !second.exists() {
        eprintln!("skipping: fixtures missing");
        return;
    }

    let mut project = Project::empty();
    register(&mut project, &first);
    register(&mut project, &second);

    // Lossless must refuse this pair.
    let ids: Vec<_> = project.sources.iter().map(|s| s.id).collect();
    let report = cf_core::media::export::check_compat(&project, &ids);
    assert!(
        !report.is_compatible(),
        "different codecs must not be losslessly concatenable"
    );

    // Precise must handle it.
    let plan = plan_precise(
        &project,
        Time::ZERO,
        Time::from_secs(7.0),
        PreciseSettings::default(),
    );
    assert!(plan.is_runnable(), "plan blocked: {:?}", plan.blocker);

    let output = std::env::temp_dir().join("clipforge-p6-mixed.mp4");
    let temp = temp_path(&output);

    let mut cmd = build_precise_command(
        &install,
        &plan,
        &[first.clone(), second.clone()],
        &temp,
        &output,
        true,
    )
    .expect("command");

    let output_result = cmd.stderr(Stdio::piped()).output().expect("run");
    assert!(
        output_result.status.success(),
        "mixed-source precise export failed: {}",
        String::from_utf8_lossy(&output_result.stderr)
    );

    std::fs::rename(&temp, &output).expect("rename");

    let codecs = ffprobe_entries(&install, &output, "stream=codec_name");
    assert!(codecs.contains("h264"), "output should be h264: {codecs}");
    assert!(
        codecs.contains("aac"),
        "output should have aac audio: {codecs}"
    );

    let duration: f64 = ffprobe_entries(&install, &output, "format=duration")
        .parse()
        .unwrap_or(0.0);
    assert!(
        (duration - 7.0).abs() < 0.8,
        "duration {duration} should be about 7 s"
    );

    let _ = std::fs::remove_file(&output);
    let _ = ClipId(0);
}
