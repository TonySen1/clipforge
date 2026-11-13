//! Audio output for preview playback.
//!
//! Design: the whole audio span is decoded up front (see
//! `cf_core::media::audio`) and handed to a cpal output stream. The callback
//! only copies samples from that buffer and advances a shared cursor, which
//! keeps it allocation-free and lock-light — cpal's callback must never block.
//!
//! Synchronisation is *cursor-driven*: video playback asks the audio layer
//! where it is, rather than the other way round. Audio is the more reliable
//! clock (a sound card's clock cannot be paused), and `ffmpeg -re` is only
//! accurate to a frame or so.
//!
//! If there is no output device the whole module degrades to a no-op: preview
//! keeps working silently instead of failing to start.

use cf_core::media::audio::{AudioBuffer, CHANNELS, SAMPLE_RATE};
use cf_core::time::Time;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared between the UI/business side and the audio callback.
struct Shared {
    /// Decoded PCM. Swapped wholesale when the playhead moves.
    buffer: Mutex<AudioBuffer>,
    /// Playback position in frames, relative to `buffer.start`.
    cursor: AtomicU64,
    /// Frames already played past the end, so the callback can detect the end.
    playing: AtomicBool,
    /// 0..=1000, applied per sample.
    volume: AtomicBelt,
    /// Set while the stream should emit silence without advancing.
    muted: AtomicBool,
}

/// `AtomicU32` wrapper with a name that says what it stores.
type AtomicBelt = std::sync::atomic::AtomicU32;

impl Shared {
    fn new() -> Self {
        Self {
            buffer: Mutex::new(AudioBuffer::default()),
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

    /// Replaces the buffer and positions the cursor at `at`.
    ///
    /// The cursor is set from a timeline position so a seek during playback
    /// resumes from exactly where the picture is.
    pub fn load(&self, buffer: AudioBuffer) {
        let start = buffer.start;
        let cursor = buffer.frame_at(start).unwrap_or(0) as u64;

        if let Ok(mut slot) = self.shared.buffer.lock() {
            *slot = buffer;
        }
        self.shared.cursor.store(cursor, Ordering::Relaxed);

        // A seek during playback should keep playing from the new spot.
        if self.shared.playing.load(Ordering::Relaxed) {
            self.shared.playing.store(true, Ordering::Relaxed);
        }
        let _ = start;
    }

    /// Starts or resumes output at the given timeline position.
    pub fn play_from(&self, at: Time) {
        let cursor = {
            let buffer = match self.shared.buffer.lock() {
                Ok(buffer) => buffer,
                Err(_) => return,
            };
            buffer.frame_at(at).unwrap_or(0) as u64
        };
        self.shared.cursor.store(cursor, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Stops advancing. The cursor keeps its position so a later `play_from`
    /// resumes cleanly.
    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// Where the audio has actually reached on the timeline.
    ///
    /// Returns `None` when there is no audio loaded, so the caller can fall
    /// back to its own clock.
    pub fn position(&self) -> Option<Time> {
        let buffer = self.shared.buffer.lock().ok()?;
        if buffer.is_empty() {
            return None;
        }
        let frames = self.shared.cursor.load(Ordering::Relaxed);
        let micros = (frames as i128 * 1_000_000 / SAMPLE_RATE as i128) as i64;
        Some(buffer.start.saturating_add(Time(micros)))
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

    /// Drops the buffer, e.g. when the source changes.
    pub fn clear(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.cursor.store(0, Ordering::Relaxed);
        if let Ok(mut slot) = self.shared.buffer.lock() {
            *slot = AudioBuffer::default();
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

    let Ok(buffer) = shared.buffer.try_lock() else {
        return;
    };
    if buffer.is_empty() {
        return;
    }

    let gain = shared.volume.load(Ordering::Relaxed) as f32 / 1000.0;
    let mut cursor = shared.cursor.load(Ordering::Relaxed) as usize;
    let total_frames = buffer.frames();

    for frame in output.chunks_mut(CHANNELS) {
        if cursor >= total_frames {
            shared.playing.store(false, Ordering::Relaxed);
            break;
        }
        let base = cursor * CHANNELS;
        for (channel, slot) in frame.iter_mut().enumerate() {
            let sample = buffer.samples.get(base + channel).copied().unwrap_or(0.0);
            *slot = sample * gain;
        }
        cursor += 1;
    }

    shared.cursor.store(cursor as u64, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!engine.shared.playing.load(Ordering::Relaxed));
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
        {
            let mut buffer = shared.buffer.lock().unwrap();
            *buffer = AudioBuffer {
                samples: vec![0.5; 8 * CHANNELS],
                start: Time::ZERO,
            };
        }
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
        {
            let mut buffer = shared.buffer.lock().unwrap();
            *buffer = AudioBuffer {
                samples: vec![0.5; 4 * CHANNELS],
                start: Time::ZERO,
            };
        }
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
    fn muted_output_is_silent_without_advancing() {
        let shared = Shared::new();
        {
            let mut buffer = shared.buffer.lock().unwrap();
            *buffer = AudioBuffer {
                samples: vec![0.5; 8 * CHANNELS],
                start: Time::ZERO,
            };
        }
        shared.playing.store(true, Ordering::Relaxed);
        shared.muted.store(true, Ordering::Relaxed);

        let mut out = vec![1.0f32; 8];
        fill(&shared, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 0);
    }
}
