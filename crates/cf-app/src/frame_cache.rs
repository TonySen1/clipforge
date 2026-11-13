//! LRU frame cache for preview seeks.
//!
//! Scrubbing tends to revisit the same neighbourhood: the user overshoots,
//! drags back, and lands within a few frames of somewhere they just were. A
//! cache hit costs a memcpy; a miss costs starting an FFmpeg process (~79 ms of
//! that is process startup alone).
//!
//! Entries are keyed by source *and* frame index, not raw microseconds, so
//! nearby times that render the same frame share one entry. Eviction is by
//! byte budget rather than entry count, because one 4K frame is worth a
//! thousand thumbnails.

use cf_core::media::Frame;
use cf_core::time::Time;
use cf_core::{FrameRate, SourceId};
use std::collections::{HashMap, VecDeque};

/// Default budget: 96 MB holds ~85 frames at 720p RGBA, or ~330 at 360p.
pub const DEFAULT_BUDGET_BYTES: usize = 96 * 1024 * 1024;

/// Cache key: whatever makes a decoded frame unique.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    source: SourceId,
    /// Frame index at the source's own rate. Two seeks inside the same frame
    /// produce identical pixels, so they must share an entry.
    frame: i64,
}

/// Insertion-ordered LRU keyed by source frame.
pub struct FrameCache {
    entries: HashMap<Key, Frame>,
    /// Recency order: front is least recently used.
    order: VecDeque<Key>,
    bytes: usize,
    budget: usize,
    hits: u64,
    misses: u64,
}

impl FrameCache {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget: budget.max(1),
            hits: 0,
            misses: 0,
        }
    }

    /// Looks up the frame covering `time`.
    ///
    /// A hit also refreshes the entry's recency, which is what makes the
    /// eviction policy LRU rather than FIFO.
    pub fn get(&mut self, source: SourceId, rate: FrameRate, time: Time) -> Option<Frame> {
        let key = Key {
            source,
            frame: rate.frame_at(time),
        };

        let Some(frame) = self.entries.get(&key).cloned() else {
            self.misses += 1;
            return None;
        };

        self.touch(key);
        self.hits += 1;
        Some(frame)
    }

    /// Stores a frame, evicting until it fits.
    ///
    /// A frame larger than the whole budget is simply not cached: keeping it
    /// would evict everything else for one picture.
    pub fn insert(&mut self, source: SourceId, rate: FrameRate, frame: Frame) {
        let key = Key {
            source,
            frame: rate.frame_at(frame.time),
        };

        if let Some(existing) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(existing.byte_len());
            self.order.retain(|candidate| *candidate != key);
        }

        let size = frame.byte_len();
        if size > self.budget {
            return;
        }

        while self.bytes + size > self.budget {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(evicted.byte_len());
            }
        }

        self.bytes += size;
        self.order.push_back(key);
        self.entries.insert(key, frame);
    }

    /// Drops everything, e.g. when every source changes size.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    #[allow(dead_code)] // used by tests and the debug status line
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[allow(dead_code)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    #[allow(dead_code)]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    fn touch(&mut self, key: Key) {
        if let Some(position) = self.order.iter().position(|candidate| *candidate == key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
    }
}

impl Default for FrameCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS30: FrameRate = FrameRate { num: 30, den: 1 };

    fn frame(seconds: f64, size: usize) -> Frame {
        Frame {
            width: (size / 4) as u32,
            height: 1,
            rgba: vec![0u8; size],
            time: Time::from_secs(seconds),
        }
    }

    fn cache(budget: usize) -> FrameCache {
        FrameCache::new(budget)
    }

    #[test]
    fn stores_and_returns_a_frame() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));

        let hit = cache.get(SourceId(1), FPS30, Time::from_secs(0.0));
        assert!(hit.is_some());
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn misses_for_unknown_frames() {
        let mut cache = cache(1024);
        assert!(cache.get(SourceId(1), FPS30, Time::ZERO).is_none());
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn a_different_source_does_not_alias() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));
        assert!(cache.get(SourceId(2), FPS30, Time::ZERO).is_none());
    }

    #[test]
    fn times_inside_one_frame_share_an_entry() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));

        // At 30 fps a frame lasts 33.3 ms, so 10 ms and 15 ms are both frame 0.
        assert!(cache
            .get(SourceId(1), FPS30, Time::from_micros(10_000))
            .is_some());
        assert!(cache
            .get(SourceId(1), FPS30, Time::from_micros(15_000))
            .is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn different_frames_are_separate_entries() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));
        cache.insert(SourceId(1), FPS30, frame(0.1, 100));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut cache = cache(250);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));
        cache.insert(SourceId(1), FPS30, frame(0.1, 100));

        // Touch the older frame so the newer one becomes the LRU victim.
        cache.get(SourceId(1), FPS30, Time::from_secs(0.0));

        cache.insert(SourceId(1), FPS30, frame(0.2, 100));

        assert!(cache
            .get(SourceId(1), FPS30, Time::from_secs(0.0))
            .is_some());
        assert!(cache
            .get(SourceId(1), FPS30, Time::from_secs(0.1))
            .is_none());
        assert!(cache
            .get(SourceId(1), FPS30, Time::from_secs(0.2))
            .is_some());
    }

    #[test]
    fn evicts_by_bytes_not_entries() {
        let mut cache = cache(300);
        cache.insert(SourceId(1), FPS30, frame(0.0, 200));
        cache.insert(SourceId(1), FPS30, frame(0.1, 100));
        assert_eq!(cache.len(), 2);

        // A third 200-byte frame cannot coexist with the first.
        cache.insert(SourceId(1), FPS30, frame(0.2, 200));
        assert_eq!(cache.bytes(), 200 + 100);
        assert!(cache.bytes() <= 300);
    }

    #[test]
    fn a_frame_larger_than_the_budget_is_skipped() {
        let mut cache = cache(100);
        cache.insert(SourceId(1), FPS30, frame(0.0, 500));
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn reinserting_the_same_frame_replaces_it() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));
        cache.insert(SourceId(1), FPS30, frame(0.0, 120));

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 120);
    }

    #[test]
    fn clear_empties_everything() {
        let mut cache = cache(1024);
        cache.insert(SourceId(1), FPS30, frame(0.0, 100));
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn budget_is_never_exceeded() {
        let mut cache = cache(500);
        for step in 0..50 {
            cache.insert(SourceId(1), FPS30, frame(step as f64 * 0.1, 70));
            assert!(cache.bytes() <= 500, "budget exceeded at step {step}");
        }
    }
}
