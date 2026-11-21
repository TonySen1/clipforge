//! Export worker.
//!
//! Runs one job at a time on a background thread. Two shapes:
//!
//! * **Lossless, one segment** — a single `-c copy` invocation.
//! * **Lossless, many segments** — each segment is copied into an MPEG-TS
//!   intermediate, then the `concat` demuxer joins them with `-c copy`.
//!   MPEG-TS is used because it concatenates without an index rewrite.
//! * **Precise** — one invocation with a `-filter_complex` graph.
//!
//! Invariants for every shape:
//!
//! 1. writes to `<output>.part` so a cancelled job never leaves a file that
//!    looks finished;
//! 2. reads `-progress pipe:1` on its own thread for the progress bar;
//! 3. drains `stderr` on *another* thread — if nobody reads it, a chatty
//!    FFmpeg fills the pipe buffer and blocks forever;
//! 4. on success renames `.part` to the final path; on cancel or failure
//!    deletes it.

use cf_core::media::export::{
    build_lossless_command, build_precise_command, build_precise_command_for, cleanup_temp,
    container_for, finalize_output, temp_path, GraphStreams, LosslessPlan, PrecisePlan,
    PreciseSettings, Segment, SmartCutPlan, TimelinePiece, VideoCodecChoice,
};
use cf_core::media::ffmpeg::{hidden_command, FfmpegInstall};
use cf_core::media::progress::{percent, stderr_summary, ProgressParser};
use cf_core::time::Time;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// A source file the job needs to read.
#[derive(Clone, Debug)]
pub struct JobSource {
    pub id: cf_core::SourceId,
    pub path: PathBuf,
    pub has_audio: bool,
}

/// Everything the worker needs to run one job.
#[derive(Debug)]
pub enum ExportJob {
    /// One clip, stream copy.
    LosslessSingle {
        label: String,
        install: Arc<FfmpegInstall>,
        source: PathBuf,
        output: PathBuf,
        plan: LosslessPlan,
        has_audio: bool,
    },
    /// Several segments, each copied to TS then concatenated.
    LosslessConcat {
        label: String,
        install: Arc<FfmpegInstall>,
        sources: Vec<JobSource>,
        /// One entry per output segment; order is export order.
        segments: Vec<Segment>,
        output: PathBuf,
        total: Time,
    },
    /// Re-encode the timeline window.
    Precise {
        label: String,
        install: Arc<FfmpegInstall>,
        sources: Vec<JobSource>,
        output: PathBuf,
        plan: PrecisePlan,
        /// Set when keyframe-aligned copying can replace most of the encode.
        smart: Option<SmartCutPlan>,
    },
}

impl ExportJob {
    pub fn output(&self) -> &Path {
        match self {
            Self::LosslessSingle { output, .. } => output,
            Self::LosslessConcat { output, .. } => output,
            Self::Precise { output, .. } => output,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::LosslessSingle { label, .. } => label,
            Self::LosslessConcat { label, .. } => label,
            Self::Precise { label, .. } => label,
        }
    }
}

/// Messages the worker posts back.
#[derive(Debug)]
pub enum ExportEvent {
    /// Periodic progress. `stage` texts are i18n keys.
    Progress {
        done: Time,
        total: Time,
        percent: f32,
        speed: Option<u32>,
    },
    Succeeded {
        output: PathBuf,
        /// Last lines of FFmpeg's stderr, for the "details" disclosure.
        log_tail: String,
    },
    Failed {
        detail: String,
        log_tail: String,
    },
    Cancelled,
}

/// Handle to the running export.
pub struct ExportWorker {
    cancel: Arc<AtomicBool>,
    events: Receiver<ExportEvent>,
    /// Set on cancel so the UI can distinguish "cancelling" from "idle".
    cancelling: bool,
}

impl ExportWorker {
    /// Starts a job. Returns immediately; poll `drain` for progress.
    pub fn start(job: ExportJob, ctx: egui::Context) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = cancel.clone();
        let (tx, events) = mpsc::channel();

        std::thread::Builder::new()
            .name("clipforge-export".to_owned())
            .spawn(move || run_job(job, cancel_flag, tx, ctx))
            .expect("spawning the export worker");

        Self {
            cancel,
            events,
            cancelling: false,
        }
    }

    /// Asks the child process to stop.
    pub fn cancel(&mut self) {
        self.cancelling = true;
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancelling
    }

    /// Drains events, keeping the newest progress update.
    pub fn drain(&self) -> ExportDrained {
        let mut newest_progress = None;
        let mut outcome = None;

        loop {
            match self.events.try_recv() {
                Ok(ExportEvent::Progress {
                    done,
                    total,
                    percent,
                    speed,
                }) => {
                    newest_progress = Some((done, total, percent, speed));
                }
                Ok(other) => outcome = Some(other),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        ExportDrained {
            progress: newest_progress,
            outcome,
        }
    }
}

pub struct ExportDrained {
    pub progress: Option<(Time, Time, f32, Option<u32>)>,
    pub outcome: Option<ExportEvent>,
}

fn run_job(
    job: ExportJob,
    cancel: Arc<AtomicBool>,
    events: Sender<ExportEvent>,
    ctx: egui::Context,
) {
    let output = job.output().to_path_buf();
    let temp = temp_path(&output);

    // Make sure the parent directory exists before ffmpeg fails on us.
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                fail(
                    &events,
                    &ctx,
                    format!("cannot create {}: {err}", parent.display()),
                    String::new(),
                );
                return;
            }
        }
    }

    log::info!("export start: {} => {}", job.label(), output.display());

    let result = match job {
        ExportJob::LosslessSingle {
            install,
            source,
            plan,
            has_audio,
            ..
        } => run_lossless_single(
            &install, &source, &temp, &output, &plan, has_audio, &cancel, &events, &ctx,
        ),
        ExportJob::LosslessConcat {
            install,
            sources,
            segments,
            total,
            ..
        } => run_lossless_concat(
            &install, &sources, &segments, total, &temp, &output, &cancel, &events, &ctx,
        ),
        ExportJob::Precise {
            install,
            sources,
            plan,
            smart,
            ..
        } => run_precise(
            &install,
            &sources,
            &plan,
            smart.as_ref(),
            &temp,
            &output,
            &cancel,
            &events,
            &ctx,
        ),
    };

    match result {
        JobResult::Succeeded { log_tail } => match finalize_output(&temp, &output) {
            Ok(()) => {
                let _ = events.send(ExportEvent::Succeeded { output, log_tail });
            }
            Err(err) => {
                cleanup_temp(&temp);
                let _ = events.send(ExportEvent::Failed {
                    detail: err.detail(),
                    log_tail,
                });
            }
        },
        JobResult::Cancelled => {
            cleanup_temp(&temp);
            let _ = events.send(ExportEvent::Cancelled);
        }
        JobResult::Failed { detail, log_tail } => {
            cleanup_temp(&temp);
            let _ = events.send(ExportEvent::Failed { detail, log_tail });
        }
    }

    ctx.request_repaint();
}

enum JobResult {
    Succeeded { log_tail: String },
    Failed { detail: String, log_tail: String },
    Cancelled,
}

fn fail(events: &Sender<ExportEvent>, ctx: &egui::Context, detail: String, log_tail: String) {
    let _ = events.send(ExportEvent::Failed { detail, log_tail });
    ctx.request_repaint();
}

#[allow(clippy::too_many_arguments)]
fn run_lossless_single(
    install: &FfmpegInstall,
    source: &Path,
    temp: &Path,
    output: &Path,
    plan: &LosslessPlan,
    has_audio: bool,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let command = build_lossless_command(install, source, temp, plan, has_audio, output);
    run_ffmpeg_with_offset(command, plan.real_duration, Time::ZERO, cancel, events, ctx)
}

#[allow(clippy::too_many_arguments)]
fn run_lossless_concat(
    install: &FfmpegInstall,
    sources: &[JobSource],
    segments: &[Segment],
    total: Time,
    temp: &Path,
    output: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let paths: HashMap<cf_core::SourceId, &Path> = sources
        .iter()
        .map(|source| (source.id, source.path.as_path()))
        .collect();
    let audio: HashMap<cf_core::SourceId, bool> = sources
        .iter()
        .map(|source| (source.id, source.has_audio))
        .collect();

    // Intermediates live next to the output so they land on the same volume.
    let work_dir = temp.with_extension("segments");
    if let Err(err) = std::fs::create_dir_all(&work_dir) {
        return JobResult::Failed {
            detail: format!("cannot create {}: {err}", work_dir.display()),
            log_tail: String::new(),
        };
    }

    let mut list = String::new();
    let mut log = String::new();
    let mut completed = Time::ZERO;

    for (index, segment) in segments.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&work_dir);
            return JobResult::Cancelled;
        }

        let Some(path) = paths.get(&segment.source_id) else {
            let _ = std::fs::remove_dir_all(&work_dir);
            return JobResult::Failed {
                detail: format!("segment {index} refers to an unknown source"),
                log_tail: log,
            };
        };
        let has_audio = audio.get(&segment.source_id).copied().unwrap_or(false);

        let segment_path = work_dir.join(format!("seg{index:03}.ts"));
        let mut cmd = hidden_command(&install.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-y", "-ss"])
            .arg(format!("{:.6}", segment.source_in.as_secs_f64()))
            .arg("-i")
            .arg(path)
            .args(["-t"])
            .arg(format!("{:.6}", segment.duration().as_secs_f64()))
            .args(["-map", "0:v:0"]);

        if has_audio {
            cmd.args(["-map", "0:a:0?"]);
        }

        cmd.args([
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            // MPEG-TS concatenates without an index rewrite, which is what
            // makes this intermediate format worth the extra pass.
            "-f",
            "mpegts",
            "-progress",
            "pipe:1",
            "-nostats",
        ])
        .arg(&segment_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        match run_ffmpeg_with_offset(cmd, total, completed, cancel, events, ctx) {
            JobResult::Succeeded { log_tail } => {
                log.push_str(&log_tail);
                completed = completed.saturating_add(segment.duration());
                list.push_str(&format!("file '{}'\n", concat_escape(&segment_path)));
            }
            other => {
                let _ = std::fs::remove_dir_all(&work_dir);
                return other;
            }
        }
    }

    if list.is_empty() {
        let _ = std::fs::remove_dir_all(&work_dir);
        return JobResult::Failed {
            detail: "no segments were produced".to_owned(),
            log_tail: log,
        };
    }

    let list_path = work_dir.join("concat.txt");
    if let Err(err) = std::fs::write(&list_path, list) {
        let _ = std::fs::remove_dir_all(&work_dir);
        return JobResult::Failed {
            detail: format!("cannot write the concat list: {err}"),
            log_tail: log,
        };
    }

    // Final join: one more stream copy over the intermediates.
    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
    ])
    .arg(&list_path)
    .args([
        "-c",
        "copy",
        "-avoid_negative_ts",
        "make_zero",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);

    if let Some(container) = container_for(output) {
        cmd.args(["-f", container]);
    }

    cmd.arg(temp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let result = run_ffmpeg_with_offset(cmd, total, completed, cancel, events, ctx);
    let _ = std::fs::remove_dir_all(&work_dir);

    match result {
        JobResult::Succeeded { log_tail } => JobResult::Succeeded {
            log_tail: format!("{log}\n{log_tail}"),
        },
        other => other,
    }
}

/// Escapes a path for FFmpeg's concat list format.
///
/// The list is single-quoted and has no escape sequences, so an embedded quote
/// is written as `'\''`. Backslashes must become forward slashes or FFmpeg
/// treats them as escapes.
fn concat_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\\''")
}

#[allow(clippy::too_many_arguments)]
fn run_precise(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    smart: Option<&SmartCutPlan>,
    temp: &Path,
    output: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let container = container_for(output).unwrap_or("mp4");
    // Smart cut is the biggest win: most of the timeline is stream-copied.
    if let Some(smart) = smart {
        log::info!(
            "precise export: smart cut, {} copied of {} ({} pieces)",
            smart.copied,
            plan.duration,
            smart.pieces.len()
        );
        return run_precise_smart(
            install, sources, plan, smart, container, temp, cancel, events, ctx,
        );
    }
    let chunks = precise_chunk_count(plan, container);
    log::info!("precise export: {} chunk(s) for {}", chunks, plan.duration);
    if chunks > 1 {
        return run_precise_chunked(
            install, sources, plan, chunks, container, temp, cancel, events, ctx,
        );
    }

    let paths: HashMap<cf_core::SourceId, &Path> = sources
        .iter()
        .map(|source| (source.id, source.path.as_path()))
        .collect();

    // Input order must match the filter graph, which numbers inputs by
    // position among the *real* segments (gaps have no input).
    let mut inputs = Vec::new();
    let mut any_audio = false;
    for segment in plan.segments() {
        let Some(path) = paths.get(&segment.source_id) else {
            return JobResult::Failed {
                detail: "a segment refers to an unknown source".to_owned(),
                log_tail: String::new(),
            };
        };
        inputs.push(path.to_path_buf());
        any_audio |= sources
            .iter()
            .find(|s| s.id == segment.source_id)
            .map(|s| s.has_audio)
            .unwrap_or(false);
    }

    let cmd = match build_precise_command(install, plan, &inputs, temp, output, any_audio) {
        Ok(cmd) => cmd,
        Err(blocker) => {
            return JobResult::Failed {
                detail: format!("{blocker:?}"),
                log_tail: String::new(),
            }
        }
    };

    run_ffmpeg_with_offset(cmd, plan.duration, Time::ZERO, cancel, events, ctx)
}

/// Shortest export worth splitting into chunks.
const CHUNK_MIN_DURATION: Time = Time(60_000_000);

/// How many parallel chunks a precise export should use.
///
/// A single x264 process leaves roughly half of a many-core CPU idle (frame
/// threading saturates on dependencies), so long exports are split into
/// independent chunks that encode concurrently. Short exports keep one process
/// because process startup and extra keyframes would outweigh the gain.
fn precise_chunk_count(plan: &PrecisePlan, container: &str) -> usize {
    let logical = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    chunk_count_for(logical, plan.duration, plan.settings.codec, container)
}

fn chunk_count_for(
    logical_cores: usize,
    duration: Time,
    codec: VideoCodecChoice,
    container: &str,
) -> usize {
    if duration < CHUNK_MIN_DURATION {
        return 1;
    }
    // MPEG-TS intermediates cannot hold VP9, and webm is not a TS target.
    if matches!(codec, VideoCodecChoice::Vp9) || !matches!(container, "mp4" | "mov" | "matroska") {
        return 1;
    }
    if logical_cores < 8 {
        return 1;
    }
    (logical_cores / 8).clamp(2, 4)
}

/// Encodes a precise export as parallel video chunks plus one continuous audio
/// pass, then joins them with stream copy.
///
/// Splitting audio off is deliberate: AAC chunks re-encoded independently pick
/// up encoder priming at every join (audible as a tiny gap), so audio is
/// encoded once over the whole timeline while the expensive video work runs in
/// parallel.
#[allow(clippy::too_many_arguments)]
fn run_precise_chunked(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    chunks: usize,
    container: &str,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let work = temp.with_extension("chunks");
    if let Err(err) = std::fs::create_dir_all(&work) {
        return JobResult::Failed {
            detail: format!("cannot create {}: {err}", work.display()),
            log_tail: String::new(),
        };
    }

    let result = run_precise_chunked_inner(
        install, sources, plan, chunks, container, &work, temp, cancel, events, ctx,
    );
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// One video piece to encode or copy before the final join.
struct VideoPiece {
    command: Command,
    path: PathBuf,
    duration: Time,
}

#[allow(clippy::too_many_arguments)]
fn run_precise_chunked_inner(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    chunks: usize,
    container: &str,
    work: &Path,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let paths: HashMap<cf_core::SourceId, &Path> = sources
        .iter()
        .map(|source| (source.id, source.path.as_path()))
        .collect();
    let any_audio = !plan.sources_with_audio.is_empty();

    let window_start = plan.start();
    let mut pieces = Vec::new();

    for index in 0..chunks {
        let from = plan.duration.micros() * index as i64 / chunks as i64;
        let to = plan.duration.micros() * (index + 1) as i64 / chunks as i64;
        if to <= from {
            continue;
        }
        let sub = plan.slice(
            window_start.saturating_add(Time(from)),
            window_start.saturating_add(Time(to)),
        );

        let mut inputs = Vec::with_capacity(sub.segments().len());
        for segment in sub.segments() {
            let Some(path) = paths.get(&segment.source_id) else {
                return JobResult::Failed {
                    detail: "a segment refers to an unknown source".to_owned(),
                    log_tail: String::new(),
                };
            };
            inputs.push(path.to_path_buf());
        }

        let chunk_path = work.join(format!("v{index:03}.ts"));
        match build_precise_command_for(
            install,
            &sub,
            &inputs,
            &chunk_path,
            "mpegts",
            any_audio,
            GraphStreams::VideoOnly,
        ) {
            Ok(command) => pieces.push(VideoPiece {
                command,
                path: chunk_path,
                duration: Time(to - from),
            }),
            Err(blocker) => {
                return JobResult::Failed {
                    detail: format!("{blocker:?}"),
                    log_tail: String::new(),
                }
            }
        }
    }

    if pieces.is_empty() {
        return JobResult::Failed {
            detail: "no chunks were planned".to_owned(),
            log_tail: String::new(),
        };
    }

    run_parallel_batch(
        install, sources, plan, pieces, container, work, temp, cancel, events, ctx,
    )
}

/// Stream-copy fast path: copies keyframe-aligned spans and only re-encodes the
/// short pieces around each cut point.
#[allow(clippy::too_many_arguments)]
fn run_precise_smart(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    smart: &SmartCutPlan,
    container: &str,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let work = temp.with_extension("smart");
    if let Err(err) = std::fs::create_dir_all(&work) {
        return JobResult::Failed {
            detail: format!("cannot create {}: {err}", work.display()),
            log_tail: String::new(),
        };
    }

    let result = run_precise_smart_inner(
        install, sources, plan, smart, container, &work, temp, cancel, events, ctx,
    );
    let _ = std::fs::remove_dir_all(&work);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_precise_smart_inner(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    smart: &SmartCutPlan,
    container: &str,
    work: &Path,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let mut pieces = Vec::with_capacity(smart.pieces.len());

    for (index, piece) in smart.pieces.iter().enumerate() {
        let Some(source_path) = sources
            .iter()
            .find(|source| source.id == piece.source_id)
            .map(|source| source.path.clone())
        else {
            return JobResult::Failed {
                detail: "a smart-cut piece refers to an unknown source".to_owned(),
                log_tail: String::new(),
            };
        };
        let duration = piece.source_out.saturating_sub(piece.source_in);
        if duration <= Time::ZERO {
            continue;
        }
        let piece_path = work.join(format!("p{index:03}.ts"));
        let command = if piece.encode {
            let Some(info) = smart.source(piece.source_id) else {
                return JobResult::Failed {
                    detail: "a smart-cut piece has no source parameters".to_owned(),
                    log_tail: String::new(),
                };
            };
            let settings = PreciseSettings {
                codec: info.codec,
                crf: plan.settings.crf,
                frame_rate: Some(info.frame_rate),
                size: Some((info.width, info.height)),
                audio_bitrate_kbps: plan.settings.audio_bitrate_kbps,
                fill_gaps: true,
                speed: plan.settings.speed,
            };
            let sub_plan = PrecisePlan {
                pieces: vec![TimelinePiece::Clip(Segment {
                    source_id: piece.source_id,
                    source_in: piece.source_in,
                    source_out: piece.source_out,
                    timeline_start: Time::ZERO,
                    timeline_end: duration,
                })],
                sources_with_audio: Vec::new(),
                duration,
                width: info.width,
                height: info.height,
                frame_rate: info.frame_rate,
                settings,
                blocker: None,
            };
            match build_precise_command_for(
                install,
                &sub_plan,
                &[source_path],
                &piece_path,
                "mpegts",
                false,
                GraphStreams::VideoOnly,
            ) {
                Ok(command) => command,
                Err(blocker) => {
                    return JobResult::Failed {
                        detail: format!("{blocker:?}"),
                        log_tail: String::new(),
                    }
                }
            }
        } else {
            build_copy_piece_command(
                install,
                &source_path,
                piece.source_in,
                duration,
                &piece_path,
            )
        };

        pieces.push(VideoPiece {
            command,
            path: piece_path,
            duration,
        });
    }

    if pieces.is_empty() {
        return JobResult::Failed {
            detail: "no smart-cut pieces were planned".to_owned(),
            log_tail: String::new(),
        };
    }

    run_parallel_batch(
        install, sources, plan, pieces, container, work, temp, cancel, events, ctx,
    )
}

/// Stream-copy command for one smart-cut span.
fn build_copy_piece_command(
    install: &FfmpegInstall,
    source: &Path,
    from: Time,
    duration: Time,
    out: &Path,
) -> Command {
    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-ss"])
        .arg(format!("{:.6}", from.as_secs_f64()))
        .arg("-i")
        .arg(source)
        .args(["-t"])
        .arg(format!("{:.6}", duration.as_secs_f64()))
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
        ])
        .args(["-progress", "pipe:1", "-nostats", "-f", "mpegts"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Runs video pieces in bounded waves, encodes audio once in parallel, then
/// joins every piece with stream copy and muxes the audio.
#[allow(clippy::too_many_arguments)]
fn run_parallel_batch(
    install: &FfmpegInstall,
    sources: &[JobSource],
    plan: &PrecisePlan,
    pieces: Vec<VideoPiece>,
    container: &str,
    work: &Path,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let paths: HashMap<cf_core::SourceId, &Path> = sources
        .iter()
        .map(|source| (source.id, source.path.as_path()))
        .collect();
    let any_audio = !plan.sources_with_audio.is_empty();

    // --- one continuous audio pass ---------------------------------------
    let audio_path = work.join("audio.m4a");
    let audio_command = {
        let mut inputs = Vec::new();
        for segment in plan.segments() {
            let Some(path) = paths.get(&segment.source_id) else {
                return JobResult::Failed {
                    detail: "a segment refers to an unknown source".to_owned(),
                    log_tail: String::new(),
                };
            };
            inputs.push(path.to_path_buf());
        }
        match build_precise_command_for(
            install,
            plan,
            &inputs,
            &audio_path,
            "ipod",
            any_audio,
            GraphStreams::AudioOnly,
        ) {
            Ok(command) => Some(command),
            Err(blocker) => {
                return JobResult::Failed {
                    detail: format!("{blocker:?}"),
                    log_tail: String::new(),
                }
            }
        }
    };

    // --- video pieces, in bounded waves ----------------------------------
    let logical = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let max_concurrent = (logical / 4).clamp(2, 8);

    let piece_paths: Vec<PathBuf> = pieces.iter().map(|piece| piece.path.clone()).collect();
    let mut all_log = String::new();
    let mut base = Time::ZERO;

    // Audio runs alongside every wave; it is cheap compared with video, but its
    // progress is still counted so the bar does not stall while it finishes.
    let audio_cell = Arc::new(Mutex::new(Time::ZERO));
    let mut audio_child = match audio_command {
        Some(command) => match spawn_batch(command, Some(audio_cell.clone()), false) {
            Ok(child) => Some(child),
            Err(err) => {
                return JobResult::Failed {
                    detail: format!("cannot start ffmpeg: {err}"),
                    log_tail: String::new(),
                }
            }
        },
        None => None,
    };

    let mut remaining = pieces;
    while !remaining.is_empty() {
        let take = max_concurrent.min(remaining.len());
        let wave: Vec<VideoPiece> = remaining.drain(..take).collect();

        let mut children = Vec::new();
        let wave_duration = wave
            .iter()
            .fold(Time::ZERO, |acc, piece| acc.saturating_add(piece.duration));
        for piece in wave {
            match spawn_batch(piece.command, Some(Arc::new(Mutex::new(Time::ZERO))), true) {
                Ok(child) => children.push(child),
                Err(err) => {
                    kill_batch(children.iter_mut());
                    kill_batch(audio_child.iter_mut());
                    return JobResult::Failed {
                        detail: format!("cannot start ffmpeg: {err}"),
                        log_tail: all_log,
                    };
                }
            }
        }

        match wait_batch(
            &mut children,
            plan.duration,
            base,
            audio_child.is_some().then_some(&audio_cell),
            cancel,
            events,
            ctx,
        ) {
            BatchOutcome::Cancelled => {
                kill_batch(audio_child.iter_mut());
                return JobResult::Cancelled;
            }
            BatchOutcome::Failed { detail, log } => {
                kill_batch(audio_child.iter_mut());
                return JobResult::Failed {
                    detail,
                    log_tail: all_log + &log,
                };
            }
            BatchOutcome::Finished { log } => {
                all_log.push_str(&log);
                base = base.saturating_add(wave_duration);
            }
        }
    }

    if let Some(audio) = audio_child.take() {
        let mut single = vec![audio];
        match wait_batch(
            &mut single,
            plan.duration,
            plan.duration,
            Some(&audio_cell),
            cancel,
            events,
            ctx,
        ) {
            BatchOutcome::Cancelled => return JobResult::Cancelled,
            BatchOutcome::Failed { detail, log } => {
                return JobResult::Failed {
                    detail,
                    log_tail: all_log + &log,
                }
            }
            BatchOutcome::Finished { log } => all_log.push_str(&log),
        }
    }

    // --- join video pieces and mux the audio ------------------------------
    let list_path = work.join("concat.txt");
    let mut list = String::new();
    for path in &piece_paths {
        list.push_str(&format!("file '{}'\n", concat_escape(path)));
    }
    if let Err(err) = std::fs::write(&list_path, list) {
        return JobResult::Failed {
            detail: format!("cannot write the concat list: {err}"),
            log_tail: all_log,
        };
    }

    let audio_ready = std::fs::metadata(&audio_path)
        .map(|meta| meta.len() > 0)
        .unwrap_or(false);

    let _ = events.send(ExportEvent::Progress {
        done: plan.duration,
        total: plan.duration,
        percent: 95.0,
        speed: None,
    });
    ctx.request_repaint();

    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
    ])
    .arg(&list_path);
    if audio_ready {
        cmd.arg("-i").arg(&audio_path);
        cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
    } else {
        cmd.args(["-map", "0:v:0"]);
    }
    cmd.args(["-c", "copy", "-avoid_negative_ts", "make_zero"]);
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.args(["-f", container]);
    cmd.arg(temp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // The join is a stream copy: it rewrites the whole file, so track it on the
    // bar (95–100 %) instead of leaving the user at 99 until it finishes.
    match run_ffmpeg_span(cmd, plan.duration, (95.0, 100.0), cancel, events, ctx) {
        JobResult::Succeeded { log_tail } => {
            all_log.push_str(&log_tail);
            JobResult::Succeeded { log_tail: all_log }
        }
        other => other,
    }
}

/// One child in a parallel batch.
struct BatchChild {
    child: Child,
    /// Video pieces report their own encoded duration here.
    done: Option<Arc<Mutex<Time>>>,
    /// Whether `done` contributes to the video fraction. The audio pass has a
    /// cell of its own but must not be summed into video duration.
    counts_as_video: bool,
    stderr: Option<std::thread::JoinHandle<String>>,
    progress: Option<std::thread::JoinHandle<()>>,
    status: Option<std::process::ExitStatus>,
}

enum BatchOutcome {
    Finished { log: String },
    Failed { detail: String, log: String },
    Cancelled,
}

fn spawn_batch(
    mut command: Command,
    done: Option<Arc<Mutex<Time>>>,
    counts_as_video: bool,
) -> std::io::Result<BatchChild> {
    let mut child = command.spawn()?;
    let stderr = drain_stderr(&mut child);
    let progress = child.stdout.take().map(|stdout| {
        let cell = done
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(Time::ZERO)));
        progress_into(stdout, cell)
    });
    Ok(BatchChild {
        child,
        done,
        counts_as_video,
        stderr,
        progress,
        status: None,
    })
}

/// Reads `-progress pipe:1` from a child into a shared duration cell.
fn progress_into(
    stdout: std::process::ChildStdout,
    cell: Arc<Mutex<Time>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut parser = ProgressParser::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(update) = parser.push(&buf[..n]) {
                        if let Some(out_time) = update.out_time {
                            if let Ok(mut cell) = cell.lock() {
                                *cell = out_time;
                            }
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

/// Kills children that are still running (used when an unrelated piece fails).
fn kill_batch<'a, I: IntoIterator<Item = &'a mut BatchChild>>(children: I) {
    for child in children {
        let _ = child.child.kill();
        let _ = child.child.wait();
        if let Some(handle) = child.progress.take() {
            let _ = handle.join();
        }
        if let Some(handle) = child.stderr.take() {
            let _ = handle.join();
        }
    }
}

/// Waits for a parallel batch, reporting aggregate progress and handling
/// cancellation.
///
/// `base` is the progress already reported by earlier waves, so a multi-wave
/// job advances monotonically. Video fills 0–85 %, the concurrent audio pass
/// 85–95 %, and the final join 95–100 %; without audio, video fills 0–95 %.
fn wait_batch(
    children: &mut [BatchChild],
    total: Time,
    base: Time,
    audio: Option<&Arc<Mutex<Time>>>,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> BatchOutcome {
    let mut reported = 0.0f32;

    loop {
        if cancel.load(Ordering::Relaxed) {
            for child in children.iter_mut() {
                let _ = child.child.kill();
                let _ = child.child.wait();
            }
            break;
        }

        let mut all_done = true;
        for child in children.iter_mut() {
            if child.status.is_none() {
                match child.child.try_wait() {
                    Ok(Some(status)) => child.status = Some(status),
                    Ok(None) | Err(_) => all_done = false,
                }
            }
        }

        let done = children
            .iter()
            .filter(|child| child.counts_as_video)
            .filter_map(|child| child.done.as_ref())
            .map(|cell| cell.lock().map(|value| *value).unwrap_or(Time::ZERO))
            .fold(base, |acc, value| acc.saturating_add(value))
            .min(total);

        let video_fraction = percent(done, total) / 100.0;
        let (video_cap, audio_share) = if audio.is_some() {
            (85.0, 10.0)
        } else {
            (95.0, 0.0)
        };
        let audio_fraction = audio
            .and_then(|cell| cell.lock().ok().map(|value| *value))
            .map(|value| percent(value, total) / 100.0)
            .unwrap_or(1.0);
        let percent_done = video_cap * video_fraction + audio_share * audio_fraction;

        if percent_done > reported + 0.05 {
            reported = percent_done;
            let _ = events.send(ExportEvent::Progress {
                done,
                total,
                percent: percent_done.min(95.0),
                speed: None,
            });
            ctx.request_repaint();
        }

        if all_done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }

    let mut log = String::new();
    for child in children.iter_mut() {
        if let Some(handle) = child.progress.take() {
            let _ = handle.join();
        }
        if let Some(handle) = child.stderr.take() {
            if let Ok(stderr) = handle.join() {
                log.push_str(&stderr_summary(&stderr, 10));
                log.push('\n');
            }
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return BatchOutcome::Cancelled;
    }

    if let Some(failed) = children
        .iter()
        .find(|child| child.status.map(|status| !status.success()).unwrap_or(true))
    {
        let code = failed
            .status
            .and_then(|status| status.code())
            .map(|code| code.to_string())
            .unwrap_or_else(|| "a signal".to_owned());
        return BatchOutcome::Failed {
            detail: format!("ffmpeg exited with {code}"),
            log,
        };
    }

    BatchOutcome::Finished { log }
}

/// Runs a command, reporting progress as `offset + out_time` out of `total`.
///
/// Multi-stage jobs use this so the bar advances across stages instead of
/// restarting at zero.
fn run_ffmpeg_with_offset(
    command: Command,
    total: Time,
    offset: Time,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    run_ffmpeg_mapped(command, total, offset, None, cancel, events, ctx)
}

/// Runs a command whose progress maps onto an explicit percent range.
///
/// Used for the final join of a chunked/smart export: it is a stream copy, so
/// its `out_time` climbs steadily while the whole file is rewritten.
fn run_ffmpeg_span(
    command: Command,
    total: Time,
    span: (f32, f32),
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    run_ffmpeg_mapped(command, total, Time::ZERO, Some(span), cancel, events, ctx)
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_mapped(
    mut command: Command,
    total: Time,
    offset: Time,
    percent_span: Option<(f32, f32)>,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> JobResult {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return JobResult::Failed {
                detail: format!("cannot start ffmpeg: {err}"),
                log_tail: String::new(),
            }
        }
    };

    let stderr_handle = drain_stderr(&mut child);
    let progress_handle =
        forward_progress(&mut child, offset, total, percent_span, cancel, events, ctx);

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return JobResult::Failed {
                    detail: format!("lost track of ffmpeg: {err}"),
                    log_tail: String::new(),
                };
            }
        }
    };

    if let Some(handle) = progress_handle {
        let _ = handle.join();
    }
    let log_full = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let log_tail = stderr_summary(&log_full, 20);

    match status {
        None => JobResult::Cancelled,
        Some(status) if status.success() => JobResult::Succeeded { log_tail },
        Some(status) => JobResult::Failed {
            detail: format!(
                "ffmpeg exited with {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".to_owned())
            ),
            log_tail,
        },
    }
}

/// Drains stderr on its own thread; an unread pipe deadlocks FFmpeg.
fn drain_stderr(child: &mut Child) -> Option<std::thread::JoinHandle<String>> {
    child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut collected = String::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                collected.push_str(&line);
                line.clear();
            }
            collected
        })
    })
}

/// Parses `-progress pipe:1` and posts updates.
fn forward_progress(
    child: &mut Child,
    offset: Time,
    total: Time,
    percent_span: Option<(f32, f32)>,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ExportEvent>,
    ctx: &egui::Context,
) -> Option<std::thread::JoinHandle<()>> {
    let stdout = child.stdout.take()?;
    let events = events.clone();
    let ctx = ctx.clone();
    let cancel = cancel.clone();

    Some(std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut parser = ProgressParser::new();
        let mut buf = [0u8; 4096];

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(update) = parser.push(&buf[..n]) {
                        let done = offset.saturating_add(update.out_time.unwrap_or(Time::ZERO));
                        let fraction = percent(done, total) / 100.0;
                        let value = match percent_span {
                            Some((low, high)) => low + (high - low) * fraction,
                            None => fraction * 100.0,
                        };
                        let _ = events.send(ExportEvent::Progress {
                            done,
                            total,
                            percent: value,
                            speed: update.speed,
                        });
                        ctx.request_repaint();
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_core::media::export::LosslessPlan;

    fn plan(seconds: f64) -> LosslessPlan {
        LosslessPlan {
            real_start: Time::ZERO,
            real_end: Time::from_secs(seconds),
            start_drift: Time::ZERO,
            end_drift: Time::ZERO,
            real_duration: Time::from_secs(seconds),
            blocker: None,
        }
    }

    #[test]
    fn temp_path_is_next_to_the_output() {
        let p = temp_path(std::path::Path::new("C:/videos/out.mp4"));
        assert!(p.to_string_lossy().contains(".out.mp4.part"));
    }

    #[test]
    fn progress_percent_matches_the_plan_duration() {
        let job_plan = plan(10.0);
        assert_eq!(percent(Time::from_secs(5.0), job_plan.real_duration), 50.0);
    }

    #[test]
    fn cancel_flag_is_observable() {
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(!cancel.load(Ordering::Relaxed));
        cancel.store(true, Ordering::Relaxed);
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn chunking_scales_with_cores_and_only_for_long_exports() {
        let long = Time::from_secs(200.0);
        let short = Time::from_secs(30.0);

        assert_eq!(chunk_count_for(32, long, VideoCodecChoice::H264, "mp4"), 4);
        assert_eq!(chunk_count_for(16, long, VideoCodecChoice::H264, "mp4"), 2);
        assert_eq!(
            chunk_count_for(8, long, VideoCodecChoice::H264, "matroska"),
            2
        );
        // Too few cores: one process already uses the whole machine.
        assert_eq!(chunk_count_for(4, long, VideoCodecChoice::H264, "mp4"), 1);
        // Short exports do not amortise process startup.
        assert_eq!(chunk_count_for(32, short, VideoCodecChoice::H264, "mp4"), 1);
        // TS chunks cannot carry VP9, and webm is out of scope.
        assert_eq!(chunk_count_for(32, long, VideoCodecChoice::Vp9, "webm"), 1);
        assert_eq!(chunk_count_for(32, long, VideoCodecChoice::H264, "webm"), 1);
    }
}
