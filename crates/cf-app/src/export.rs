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
    build_lossless_command, build_precise_command, cleanup_temp, container_for, finalize_output,
    temp_path, LosslessPlan, PrecisePlan, Segment,
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
use std::sync::Arc;

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
            ..
        } => run_precise(
            &install, &sources, &plan, &temp, &output, &cancel, &events, &ctx,
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

/// Runs a command, reporting progress as `offset + out_time` out of `total`.
///
/// Multi-stage jobs use this so the bar advances across stages instead of
/// restarting at zero.
fn run_ffmpeg_with_offset(
    mut command: Command,
    total: Time,
    offset: Time,
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
    let progress_handle = forward_progress(&mut child, offset, total, cancel, events, ctx);

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
                        let _ = events.send(ExportEvent::Progress {
                            done,
                            total,
                            percent: percent(done, total),
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
}
