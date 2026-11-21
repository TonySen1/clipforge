//! Preview decode worker.
//!
//! Two responsibilities, kept deliberately separate:
//!
//! * **Seek decoding** runs on the worker thread itself. Each request is one
//!   short FFmpeg process (~85 ms measured), so a blocking call is fine.
//! * **Playback** runs on its own thread. `ffmpeg -re` paces itself, so the
//!   thread simply blocks on reads; stopping sets a flag and the thread exits
//!   within one frame (~33 ms).
//!
//! The UI thread never decodes and never blocks on FFmpeg.
//!
//! Staleness is handled with a generation counter: every request carries the
//! current generation, and the UI drops frames whose generation is not the one
//! it is waiting for. That makes "latest wins" fall out for free.

use crate::frame_cache::FrameCache;
use cf_core::media::{
    decode_frame, preview_size, FfmpegInstall, Frame, KeyframeStrip, MediaInfo, PlaybackProcess,
    PreviewSize,
};
use cf_core::time::Time;
use cf_core::SourceId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// What the UI asks the worker to do.
#[derive(Debug)]
pub enum PreviewRequest {
    /// Decode one frame. `precise` selects the frame-accurate (slower) seek.
    Seek {
        generation: u64,
        time: Time,
        precise: bool,
    },
    /// Start continuous playback from `from`.
    Play {
        generation: u64,
        from: Time,
    },
    Stop,
    /// Switch sources. `max_edge` caps the decoded resolution.
    Source {
        id: SourceId,
        install: Arc<FfmpegInstall>,
        path: PathBuf,
        info: Arc<MediaInfo>,
        max_edge: u32,
        /// Scrub strip, when one has been decoded for this source.
        thumbs: Option<Arc<KeyframeStrip>>,
    },
    /// The scrub strip finished decoding after the source was set.
    Thumbs {
        id: SourceId,
        strip: Arc<KeyframeStrip>,
    },
    /// Drops cached frames, e.g. after the preview quality changed.
    ClearCache,
    Shutdown,
}

/// What the worker reports back.
#[derive(Debug)]
pub enum PreviewEvent {
    Frame {
        generation: u64,
        frame: Frame,
        size: PreviewSize,
        playing: bool,
    },
    Failed {
        generation: u64,
        detail: String,
    },
    PlaybackFinished,
}

/// Handle to the worker thread.
pub struct PreviewWorker {
    tx: Sender<PreviewRequest>,
    events: Receiver<PreviewEvent>,
}

impl PreviewWorker {
    pub fn spawn(ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<PreviewRequest>();
        let (event_tx, events) = mpsc::channel::<PreviewEvent>();

        std::thread::Builder::new()
            .name("clipforge-preview".to_owned())
            .spawn(move || run_loop(rx, event_tx, ctx))
            .expect("spawning the preview worker");

        Self { tx, events }
    }

    pub fn send(&self, request: PreviewRequest) {
        let _ = self.tx.send(request);
    }

    /// Drains pending events, collapsing any burst of frames into the newest.
    pub fn drain(&self) -> DrainedEvents {
        let mut newest_frame = None;
        let mut failures = Vec::new();
        let mut finished = false;

        loop {
            match self.events.try_recv() {
                Ok(PreviewEvent::Frame {
                    generation,
                    frame,
                    size,
                    playing,
                }) => {
                    newest_frame = Some(IncomingFrame {
                        generation,
                        frame,
                        size,
                        playing,
                    });
                }
                Ok(PreviewEvent::Failed { generation, detail }) => {
                    failures.push((generation, detail));
                }
                Ok(PreviewEvent::PlaybackFinished) => finished = true,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        DrainedEvents {
            newest_frame,
            failures,
            finished,
        }
    }
}

/// One frame on its way to the UI.
pub struct IncomingFrame {
    pub generation: u64,
    pub frame: Frame,
    pub size: PreviewSize,
    pub playing: bool,
}

pub struct DrainedEvents {
    pub newest_frame: Option<IncomingFrame>,
    pub failures: Vec<(u64, String)>,
    pub finished: bool,
}

struct ActiveSource {
    id: SourceId,
    install: Arc<FfmpegInstall>,
    path: PathBuf,
    info: Arc<MediaInfo>,
    size: PreviewSize,
    thumbs: Option<Arc<KeyframeStrip>>,
}

/// A running playback thread.
struct Playback {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Playback {
    fn stop(self) {
        // The thread checks this flag after every frame; with `-re` a blocked
        // read returns within one frame interval, so joining is quick.
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn run_loop(rx: Receiver<PreviewRequest>, event_tx: Sender<PreviewEvent>, ctx: egui::Context) {
    let mut source: Option<ActiveSource> = None;
    let mut playback: Option<Playback> = None;
    // Survives across seeks: scrubbing revisits neighbours constantly, and a
    // hit avoids starting an FFmpeg process entirely.
    let mut cache = FrameCache::default();
    // Requests read from the channel before they are handled. Keeping them
    // here (rather than handling straight from `recv`) lets the loop see what
    // else is waiting and skip Seek requests a newer one has superseded.
    let mut pending: std::collections::VecDeque<PreviewRequest> = std::collections::VecDeque::new();

    loop {
        let request = match pending.pop_front() {
            Some(request) => request,
            None => match rx.recv() {
                Ok(request) => request,
                Err(_) => break,
            },
        };
        // Pull in everything already queued so the staleness check below is
        // accurate. A drag produces one Seek per pointer move.
        while let Ok(request) = rx.try_recv() {
            pending.push_back(request);
        }

        // A seek is stale when a newer seek is already waiting behind it;
        // decoding it would only delay the position the user actually wants.
        if let PreviewRequest::Seek { generation, .. } = &request {
            let superseded = pending.iter().any(|queued| {
                matches!(queued, PreviewRequest::Seek { generation: newer, .. } if newer > generation)
            });
            if superseded {
                log::debug!("skipping stale seek {generation}");
                continue;
            }
        }

        match request {
            PreviewRequest::Shutdown => {
                if let Some(playback) = playback.take() {
                    playback.stop();
                }
                break;
            }
            PreviewRequest::Source {
                id,
                install,
                path,
                info,
                max_edge,
                thumbs,
            } => {
                if let Some(playback) = playback.take() {
                    playback.stop();
                }
                let size = preview_size(&info, max_edge);
                source = Some(ActiveSource {
                    id,
                    install,
                    path,
                    info,
                    size,
                    thumbs,
                });
            }
            PreviewRequest::Thumbs { id, strip } => {
                // Only meaningful when it belongs to the source in use; a strip
                // for a source the worker moved away from would never be asked
                // for and is dropped.
                if let Some(active) = &mut source {
                    if active.id == id {
                        active.thumbs = Some(strip);
                    }
                }
            }
            PreviewRequest::ClearCache => {
                cache.clear();
            }
            PreviewRequest::Stop => {
                if let Some(playback) = playback.take() {
                    playback.stop();
                }
            }
            PreviewRequest::Seek {
                generation,
                time,
                precise,
            } => {
                if let Some(playback) = playback.take() {
                    playback.stop();
                }
                let Some(active) = &source else { continue };
                let rate = active.info.video.frame_rate;

                // A cached frame is indistinguishable from a decoded one, so
                // answer immediately — this is what keeps scrubbing responsive
                // on a clip the user has already looked at.
                if let Some(mut frame) = cache.get(active.id, rate, time) {
                    frame.time = time;
                    let _ = event_tx.send(PreviewEvent::Frame {
                        generation,
                        frame,
                        size: active.size,
                        playing: false,
                    });
                    ctx.request_repaint();
                    continue;
                }

                // The strip frame for a scrub is painted by the app directly, so
                // this worker only has to produce the exact frame. Precise
                // seeks (frame stepping, release) take the same path.

                match decode_frame(
                    &active.install,
                    &active.path,
                    &active.info,
                    time,
                    active.size,
                    precise,
                ) {
                    Ok(frame) => {
                        cache.insert(active.id, rate, frame.clone());
                        let _ = event_tx.send(PreviewEvent::Frame {
                            generation,
                            frame,
                            size: active.size,
                            playing: false,
                        });
                    }
                    Err(err) => {
                        let _ = event_tx.send(PreviewEvent::Failed {
                            generation,
                            detail: err.detail(),
                        });
                    }
                }
                ctx.request_repaint();
            }
            PreviewRequest::Play { generation, from } => {
                if let Some(playback) = playback.take() {
                    playback.stop();
                }
                let Some(active) = &source else { continue };

                let install = active.install.clone();
                let path = active.path.clone();
                let info = active.info.clone();
                let size = active.size;
                let stop = Arc::new(AtomicBool::new(false));
                let stop_flag = stop.clone();
                let events = event_tx.clone();
                let ctx = ctx.clone();

                let handle = std::thread::Builder::new()
                    .name("clipforge-playback".to_owned())
                    .spawn(move || {
                        playback_thread(
                            install, path, info, from, size, generation, stop_flag, events, ctx,
                        )
                    })
                    .expect("spawning the playback thread");

                playback = Some(Playback { stop, handle });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn playback_thread(
    install: Arc<FfmpegInstall>,
    path: PathBuf,
    info: Arc<MediaInfo>,
    from: Time,
    size: PreviewSize,
    generation: u64,
    stop: Arc<AtomicBool>,
    events: Sender<PreviewEvent>,
    ctx: egui::Context,
) {
    let process = match PlaybackProcess::spawn(&install, &path, &info, from, size) {
        Ok(process) => process,
        Err(err) => {
            let _ = events.send(PreviewEvent::Failed {
                generation,
                detail: err.detail(),
            });
            ctx.request_repaint();
            return;
        }
    };

    let mut process = process;
    let mut buf = Vec::new();

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match process.next_frame(&mut buf) {
            Some(frame) => {
                // Dropping the event when the UI has moved on is cheaper than
                // queueing frames nobody will render.
                if events
                    .send(PreviewEvent::Frame {
                        generation,
                        frame,
                        size,
                        playing: true,
                    })
                    .is_err()
                {
                    return;
                }
                ctx.request_repaint();
            }
            None => {
                let _ = events.send(PreviewEvent::PlaybackFinished);
                ctx.request_repaint();
                return;
            }
        }
    }
}

/// Maps a timeline position and frame rate to the nearest frame boundary.
#[allow(dead_code)] // used by P3 scrubbing
pub fn snap_to_frame(time: Time, frame_rate: cf_core::FrameRate) -> Time {
    let frame = frame_rate.frame_at(time);
    let snapped = frame_rate.time_of_frame(frame.max(0));
    snapped.max(Time::ZERO)
}

/// Steps exactly one frame forward or backward.
pub fn step_frames(time: Time, frame_rate: cf_core::FrameRate, delta: i64, duration: Time) -> Time {
    let current = frame_rate.frame_at(time);
    let target = (current + delta).max(0);
    let stepped = frame_rate.time_of_frame(target);
    stepped.clamp(Time::ZERO, duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_core::FrameRate;

    const FPS30: FrameRate = FrameRate { num: 30, den: 1 };

    /// Drives a real worker: a burst of seeks must be coalesced to the newest,
    /// so scrubbing never waits behind decodes the user has moved past.
    #[test]
    #[ignore = "requires a real FFmpeg and the sample fixture"]
    fn queued_seeks_are_coalesced_to_the_newest() {
        use cf_core::media::{discover, probe};

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/media/sample_h264.mp4");
        let (install, _) = discover(None).expect("FFmpeg is required");
        let install = Arc::new(install);
        let info = Arc::new(probe::probe(&install, &fixture).expect("probe"));

        let worker = PreviewWorker::spawn(egui::Context::default());
        worker.send(PreviewRequest::Source {
            id: SourceId(1),
            install,
            path: fixture,
            info,
            max_edge: 360,
            thumbs: None,
        });

        // A drag: one seek per pointer move, generations only ever increase.
        // The worker must answer the last one and skip the rest, otherwise the
        // picture lags behind the pointer by every queued decode.
        const SEEK_COUNT: u64 = 12;
        for generation in 1..=SEEK_COUNT {
            worker.send(PreviewRequest::Seek {
                generation,
                time: Time::from_micros(200_000 * generation as i64),
                precise: false,
            });
        }

        let mut answered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if let Some(frame) = worker.drain().newest_frame {
                answered.push(frame.generation);
                if frame.generation == SEEK_COUNT {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        worker.send(PreviewRequest::Shutdown);

        assert_eq!(
            answered.last().copied(),
            Some(SEEK_COUNT),
            "the newest seek must be answered, got {answered:?}"
        );
        assert!(
            answered.len() < SEEK_COUNT as usize,
            "stale seeks should be skipped, but every one was decoded: {answered:?}"
        );
    }

    #[test]
    fn snaps_to_nearest_frame() {
        // 16.5 ms is closer to frame 0 (0 ms) than to frame 1 (33.3 ms).
        assert_eq!(snap_to_frame(Time(16_500), FPS30), Time(0));
        assert_eq!(snap_to_frame(Time(20_000), FPS30), Time(33_333));
        assert_eq!(snap_to_frame(Time(0), FPS30), Time(0));
    }

    #[test]
    fn steps_forward_one_frame() {
        assert_eq!(step_frames(Time(0), FPS30, 1, Time::MAX), Time(33_333));
        assert_eq!(step_frames(Time(33_333), FPS30, 1, Time::MAX), Time(66_666));
    }

    #[test]
    fn steps_backward_one_frame() {
        assert_eq!(
            step_frames(Time(66_666), FPS30, -1, Time::MAX),
            Time(33_333)
        );
    }

    #[test]
    fn stepping_never_goes_negative() {
        assert_eq!(step_frames(Time(0), FPS30, -1, Time::MAX), Time::ZERO);
        assert_eq!(step_frames(Time(5_000), FPS30, -5, Time::MAX), Time::ZERO);
    }

    #[test]
    fn stepping_clamps_to_duration() {
        let duration = Time(1_000_000);
        assert_eq!(step_frames(Time(990_000), FPS30, 10, duration), duration);
    }

    #[test]
    fn ntsc_stepping_is_exact() {
        let ntsc = FrameRate {
            num: 30000,
            den: 1001,
        };
        let one = step_frames(Time(0), ntsc, 1, Time::MAX);
        assert_eq!(one, Time(33_366));
        let back = step_frames(one, ntsc, -1, Time::MAX);
        assert_eq!(back, Time::ZERO);
    }
}
