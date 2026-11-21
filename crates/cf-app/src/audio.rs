//! Audio output for preview playback.
//!
//! Design: decoding runs on a worker thread in **bounded chunks** (see
//! [`AudioWorker`]) and reaches this module as fully decoded PCM. The output
//! callback only copies samples from the queued buffers and advances a shared
//! cursor, which keeps it allocation-free and lock-light — cpal's callback must
//! never block.
//!
//! Synchronisation is *cursor-driven*: video playback asks the audio layer
//! where it is, rather than the other way round. Audio is the more reliable
//! clock (a sound card's clock cannot be paused), and `ffmpeg -re` is only
//! accurate to a frame or so.
//!
//! If there is no output device the whole module degrades to a no-op: preview
//! keeps working silently instead of failing to start.

use cf_core::media::audio::{AudioBuffer, CHANNELS, SAMPLE_RATE};
use cf_core::media::FfmpegInstall;
use cf_core::time::Time;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Shared between the UI/business side and the audio callback.
struct Shared {
    /// Decoded PCM, played front to back. A chunk boundary is a plain
    /// `pop_front`, so playback continues seamlessly as long as the next chunk
    /// was prefetched.
    buffers: Mutex<VecDeque<AudioBuffer>>,
    /// Playback position in frames, relative to `buffers.front().start`.
    cursor: AtomicU64,
    /// True while the callback should advance (and emit sound).
    playing: AtomicBool,
    /// 0..=1000, applied per sample.
    volume: AtomicBelt,
    /// Set while the stream should emit silence without advancing.
    muted: AtomicBool,
}

/// `AtomicU32` wrapper with a name that says what it stores.
type AtomicBelt = AtomicU32;

impl Shared {
    fn new() -> Self {
        Self {
            buffers: Mutex::new(VecDeque::new()),
            cursor: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            volume: AtomicBelt::new(1000),
            muted: AtomicBool::new(false),
        }
    }
}

/// Handle to the audio output. Cheap to keep around even when silent.
pub struct AudioEngine {
    shared: Arc<Shared>,
    stream: Option<cpal::Stream>,
    /// Why audio is unavailable, for the UI to explain once.
    unavailable: Option<String>,
}

impl AudioEngine {
    /// Opens the default output device. Never fails: on any error the engine
    /// runs in silent mode.
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(engine) => engine,
            Err(reason) => {
                log::warn!("audio output unavailable: {reason}");
                Self {
                    shared: Arc::new(Shared::new()),
                    stream: None,
                    unavailable: Some(reason),
                }
            }
        }
    }

    fn try_new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_owned())?;

        let config = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        let shared = Arc::new(Shared::new());
        let callback_shared = shared.clone();

        let stream = device
            .build_output_stream(
                config,
                move |output: &mut [f32], _info| fill(&callback_shared, output),
                |err| log::warn!("audio stream error: {err}"),
                None,
            )
            .map_err(|err| format!("cannot open the output stream: {err}"))?;

        stream
            .play()
            .map_err(|err| format!("cannot start the output stream: {err}"))?;

        Ok(Self {
            shared,
            stream: Some(stream),
            unavailable: None,
        })
    }

    pub fn is_available(&self) -> bool {
        self.stream.is_some()
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Queues a decoded chunk.
    ///
    /// Chunks arrive in timeline order with a prefetch of several seconds, so
    /// the callback can simply move to the next one when the current runs out.
    /// A chunk that starts *before* the queued audio is treated as a seek: the
    /// queue is dropped so stale audio never plays after jumping backwards.
    pub fn push(&self, buffer: AudioBuffer) {
        let Ok(mut buffers) = self.shared.buffers.lock() else {
            return;
        };

        // A chunk that starts before what is queued means the user seeked
        // backwards: the old audio would play over the new position.
        let backwards = buffers.back().is_some_and(|back| buffer.start < back.start);
        if backwards {
            buffers.clear();
            self.shared.cursor.store(0, Ordering::Relaxed);
        }
        buffers.push_back(buffer);

        // Keep at most one chunk of lookahead. A played-out front is dropped
        // (the cursor is rebased onto the next chunk); otherwise the newest
        // chunk gives way, so audio that is about to play is never cut.
        while buffers.len() > 2 {
            let cursor = self.shared.cursor.load(Ordering::Relaxed) as usize;
            match buffers.front() {
                Some(front) if cursor >= front.frames() => {
                    let played = front.frames();
                    buffers.pop_front();
                    self.shared
                        .cursor
                        .store((cursor - played) as u64, Ordering::Relaxed);
                }
                _ => {
                    buffers.pop_back();
                }
            }
        }
    }

    /// Starts or resumes output at the given timeline position.
    ///
    /// The cursor is placed inside whichever queued chunk covers `at`; when no
    /// chunk does yet, playback starts at the front of the queue, so the next
    /// frame comparison re-syncs the picture instead of stalling.
    pub fn play_from(&self, at: Time) {
        let cursor = {
            let Ok(buffers) = self.shared.buffers.lock() else {
                return;
            };
            match buffers.front() {
                Some(front) => front.frame_at(at).unwrap_or(0) as u64,
                None => 0,
            }
        };
        self.shared.cursor.store(cursor, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Stops advancing. The cursor keeps its position so a later `play_from`
    /// resumes cleanly.
    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Whether queued audio covers `at`, i.e. playback can start without a
    /// decode. Used to keep pause/resume instant.
    pub fn covers(&self, at: Time) -> bool {
        let Ok(buffers) = self.shared.buffers.lock() else {
            return false;
        };
        buffers
            .iter()
            .any(|buffer| at >= buffer.start && at < buffer.start.saturating_add(buffer.duration()))
    }

    /// Where the audio has actually reached on the timeline.
    ///
    /// Returns `None` when no audio is queued, so the caller can fall back to
    /// its own clock.
    pub fn position(&self) -> Option<Time> {
        let buffers = self.shared.buffers.lock().ok()?;
        let front = buffers.front()?;
        if front.is_empty() {
            return None;
        }
        let frames = self.shared.cursor.load(Ordering::Relaxed);
        let micros = (frames as i128 * 1_000_000 / SAMPLE_RATE as i128) as i64;
        Some(front.start.saturating_add(Time(micros)))
    }

    pub fn set_volume(&self, volume: f32) {
        let scaled = (volume.clamp(0.0, 1.0) * 1000.0).round() as u32;
        self.shared.volume.store(scaled, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.shared.playing.store(false, Ordering::Relaxed);
        }
    }

    /// Drops queued audio, e.g. when the source or position changes.
    pub fn clear(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.cursor.store(0, Ordering::Relaxed);
        if let Ok(mut buffers) = self.shared.buffers.lock() {
            buffers.clear();
        }
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Fills one output callback.
///
/// Runs on the audio thread: no allocation, no logging, and never blocks on the
/// mutex except for the short copy it needs. `try_lock` failure means a swap is
/// in flight, so this callback emits silence for one buffer instead of
/// stalling the device.
fn fill(shared: &Shared, output: &mut [f32]) {
    output.fill(0.0);

    if !shared.playing.load(Ordering::Relaxed) || shared.muted.load(Ordering::Relaxed) {
        return;
    }

    let Ok(mut buffers) = shared.buffers.try_lock() else {
        return;
    };

    let gain = shared.volume.load(Ordering::Relaxed) as f32 / 1000.0;
    let mut cursor = shared.cursor.load(Ordering::Relaxed) as usize;
    let mut written = 0;

    while written < output.len() {
        // Move past a exhausted chunk (a boundary between prefetched pieces).
        while buffers
            .front()
            .is_some_and(|front| cursor >= front.frames())
        {
            buffers.pop_front();
            cursor = 0;
        }

        let Some(front) = buffers.front() else {
            shared.playing.store(false, Ordering::Relaxed);
            break;
        };

        let out_frames = (output.len() - written) / CHANNELS;
        let take = out_frames.min(front.frames() - cursor);
        let source_base = cursor * CHANNELS;
        let dest = &mut output[written..written + take * CHANNELS];
        let source = &front.samples[source_base..source_base + take * CHANNELS];
        for (slot, sample) in dest.iter_mut().zip(source.iter()) {
            *slot = sample * gain;
        }

        written += take * CHANNELS;
        cursor += take;
    }

    shared.cursor.store(cursor as u64, Ordering::Relaxed);
}

// --- decode worker ------------------------------------------------------

/// One audio decode request: `span` of `path` starting at `source_start`,
/// destined to be played at `timeline_start`.
#[derive(Debug)]
pub struct AudioDecodeJob {
    pub generation: u64,
    pub install: Arc<FfmpegInstall>,
    pub path: PathBuf,
    pub source_start: Time,
    pub span: Time,
    pub timeline_start: Time,
}

/// Decoded audio headed back to the UI thread.
#[derive(Debug)]
pub struct DecodedAudio {
    pub generation: u64,
    /// Already re-based onto the timeline (`start` is `timeline_start`).
    pub buffer: AudioBuffer,
    /// Set when decoding failed; the buffer is then empty.
    pub failed: Option<String>,
}

/// Background audio decoder.
///
/// Decoding the *whole* remaining clip up front used to happen synchronously on
/// the UI thread, which froze the picture for seconds on long files. Requests
/// are now bounded chunks handled off-thread; the app prefetches the next chunk
/// while the current one plays.
pub struct AudioWorker {
    tx: mpsc::Sender<AudioDecodeJob>,
    events: mpsc::Receiver<DecodedAudio>,
}

impl AudioWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<AudioDecodeJob>();
        let (event_tx, events) = mpsc::channel::<DecodedAudio>();

        std::thread::Builder::new()
            .name("clipforge-audio-decode".to_owned())
            .spawn(move || run_audio_worker(rx, event_tx))
            .expect("spawning the audio decode worker");

        Self { tx, events }
    }

    pub fn request(&self, job: AudioDecodeJob) {
        let _ = self.tx.send(job);
    }

    /// Newest result per generation; older ones were superseded by a seek.
    pub fn drain(&self) -> Vec<DecodedAudio> {
        let mut out = Vec::new();
        while let Ok(decoded) = self.events.try_recv() {
            out.push(decoded);
        }
        out
    }
}

fn run_audio_worker(rx: mpsc::Receiver<AudioDecodeJob>, events: mpsc::Sender<DecodedAudio>) {
    while let Ok(job) = rx.recv() {
        // Collapse a burst of requests: only the newest one describes where the
        // user actually is, and each decode is short but not free.
        let mut job = job;
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }

        let (buffer, failed) = match cf_core::media::audio::decode(
            &job.install,
            &job.path,
            job.source_start,
            job.span,
        ) {
            Ok(mut buffer) => {
                buffer.start = job.timeline_start;
                (buffer, None)
            }
            Err(err) => (
                AudioBuffer {
                    samples: Vec::new(),
                    start: job.timeline_start,
                },
                Some(err.detail()),
            ),
        };

        if events
            .send(DecodedAudio {
                generation: job.generation,
                buffer,
                failed,
            })
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(start: f64, frames: usize, value: f32) -> AudioBuffer {
        AudioBuffer {
            samples: vec![value; frames * CHANNELS],
            start: Time::from_secs(start),
        }
    }

    #[test]
    fn engine_constructs_even_without_a_device() {
        // The constructor must never panic; on CI machines there may be no
        // audio device at all.
        let engine = AudioEngine::new();
        let _ = engine.is_available();
        let _ = engine.unavailable_reason();
    }

    #[test]
    fn silent_engine_reports_no_position() {
        let engine = AudioEngine {
            shared: Arc::new(Shared::new()),
            stream: None,
            unavailable: Some("test".into()),
        };
        engine.clear();
        assert_eq!(engine.position(), None);
        assert!(!engine.is_playing());
    }

    #[test]
    fn volume_clamps_to_range() {
        let shared = Shared::new();
        let engine = AudioEngine {
            shared: Arc::new(shared),
            stream: None,
            unavailable: None,
        };
        engine.set_volume(2.0);
        assert_eq!(engine.shared.volume.load(Ordering::Relaxed), 1000);
        engine.set_volume(-1.0);
        assert_eq!(engine.shared.volume.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn fill_respects_playing_flag() {
        let shared = Shared::new();
        // Not playing: the callback must emit silence and not advance.
        let mut out = vec![1.0f32; 8];
        fill(&shared, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn fill_advances_and_applies_gain() {
        let shared = Shared::new();
        shared.buffers.lock().unwrap().push_back(chunk(0.0, 8, 0.5));
        shared.volume.store(500, Ordering::Relaxed); // 50%
        shared.playing.store(true, Ordering::Relaxed);

        let mut out = vec![0.0f32; 8];
        fill(&shared, &mut out);

        assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6), "{out:?}");
        // 8 interleaved f32 values at 2 channels = 4 frames.
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn fill_stops_at_the_end_of_the_buffer() {
        let shared = Shared::new();
        shared.buffers.lock().unwrap().push_back(chunk(0.0, 4, 0.5));
        shared.playing.store(true, Ordering::Relaxed);

        let mut out = vec![0.0f32; 16];
        fill(&shared, &mut out);

        // The first 4 frames are audio, the rest silence.
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[7] - 0.5).abs() < 1e-6);
        assert_eq!(out[8], 0.0);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn fill_continues_into_the_next_chunk() {
        let shared = Shared::new();
        {
            let mut buffers = shared.buffers.lock().unwrap();
            buffers.push_back(chunk(0.0, 2, 0.5));
            buffers.push_back(chunk(1.0, 2, 0.25));
        }
        shared.playing.store(true, Ordering::Relaxed);

        let mut out = vec![0.0f32; 16];
        fill(&shared, &mut out);

        // 8 frames requested: 2 from the first chunk, 2 from the second, then
        // the queue runs dry and the rest is silence.
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[3] - 0.5).abs() < 1e-6);
        assert!((out[4] - 0.25).abs() < 1e-6);
        assert!((out[7] - 0.25).abs() < 1e-6);
        assert_eq!(out[8], 0.0);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn muted_output_is_silent_without_advancing() {
        let shared = Shared::new();
        shared.buffers.lock().unwrap().push_back(chunk(0.0, 8, 0.5));
        shared.playing.store(true, Ordering::Relaxed);
        shared.muted.store(true, Ordering::Relaxed);

        let mut out = vec![1.0f32; 8];
        fill(&shared, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn push_keeps_the_playing_chunk_when_overfilled() {
        let engine = AudioEngine {
            shared: Arc::new(Shared::new()),
            stream: None,
            unavailable: None,
        };
        engine.push(chunk(0.0, 4, 0.5));
        engine.push(chunk(1.0, 4, 0.5));
        engine.push(chunk(2.0, 4, 0.5));

        // Nothing has played yet, so the front chunk stays; the newest is
        // dropped instead of cutting audio that is about to play.
        let buffers = engine.shared.buffers.lock().unwrap();
        assert_eq!(buffers.len(), 2);
        assert_eq!(buffers.front().unwrap().start, Time::ZERO);
        assert_eq!(buffers.back().unwrap().start, Time::from_secs(1.0));
    }

    #[test]
    fn push_drops_a_played_out_front() {
        let engine = AudioEngine {
            shared: Arc::new(Shared::new()),
            stream: None,
            unavailable: None,
        };
        engine.push(chunk(0.0, 4, 0.5));
        engine.shared.cursor.store(4, Ordering::Relaxed); // front fully played
        engine.push(chunk(1.0, 4, 0.5));
        engine.push(chunk(2.0, 4, 0.5));

        let buffers = engine.shared.buffers.lock().unwrap();
        assert_eq!(buffers.len(), 2);
        assert_eq!(buffers.front().unwrap().start, Time::from_secs(1.0));
        assert_eq!(engine.shared.cursor.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn push_backwards_drops_stale_audio() {
        let engine = AudioEngine {
            shared: Arc::new(Shared::new()),
            stream: None,
            unavailable: None,
        };
        engine.push(chunk(10.0, 4, 0.5));
        engine.push(chunk(2.0, 4, 0.5));

        let buffers = engine.shared.buffers.lock().unwrap();
        assert_eq!(buffers.len(), 1);
        assert_eq!(buffers.front().unwrap().start, Time::from_secs(2.0));
    }

    #[test]
    fn position_follows_the_cursor() {
        let engine = AudioEngine {
            shared: Arc::new(Shared::new()),
            stream: None,
            unavailable: None,
        };
        engine.push(chunk(5.0, SAMPLE_RATE as usize, 0.0));
        engine.play_from(Time::from_secs(5.5));

        let position = engine.position().expect("position");
        assert!((position.as_secs_f64() - 5.5).abs() < 1e-3);
    }
}
