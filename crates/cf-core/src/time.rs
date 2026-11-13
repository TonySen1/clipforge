use serde::{Deserialize, Serialize};
use std::fmt;

/// A point in time or a duration, stored as whole microseconds.
///
/// Microseconds match FFmpeg's `AV_TIME_BASE` (1/1_000_000 s), which keeps
/// conversions from container time bases lossless for practical purposes.
/// Never accumulate time as `f32`/`f64`; convert at the boundary instead.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub struct Time(pub i64);

impl Time {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(i64::MAX);

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    pub const fn micros(self) -> i64 {
        self.0
    }

    pub fn from_secs(secs: f64) -> Self {
        if !secs.is_finite() {
            return Self::ZERO;
        }
        Self((secs * 1_000_000.0).round() as i64)
    }

    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Parses a decimal seconds string exactly, without a float round trip.
    ///
    /// Accepts forms such as `12`, `12.5`, `-0.021`. Extra fractional digits
    /// beyond microseconds are truncated (not rounded) to keep parsing
    /// deterministic for ffprobe output.
    pub fn parse_secs(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        let (negative, body) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };

        let (whole, fraction) = match body.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (body, ""),
        };

        let whole: i64 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };

        let mut micros = 0i64;
        let mut scale = 100_000i64;
        for ch in fraction.chars().take(6) {
            let digit = ch.to_digit(10)? as i64;
            micros += digit * scale;
            scale /= 10;
        }

        let total = whole.checked_mul(1_000_000)?.checked_add(micros)?;
        Some(Self(if negative { -total } else { total }))
    }

    pub fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }

    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }

    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub fn clamp(self, min: Self, max: Self) -> Self {
        Self(self.0.clamp(min.0, max.0))
    }

    pub fn is_negative(self) -> bool {
        self.0 < 0
    }
}

impl fmt::Display for Time {
    /// Formats as `HH:MM:SS.mmm` for UI and logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let total_ms = self.0.unsigned_abs() / 1_000;
        let ms = total_ms % 1_000;
        let total_secs = total_ms / 1_000;
        let secs = total_secs % 60;
        let mins = (total_secs / 60) % 60;
        let hours = total_secs / 3_600;
        write!(f, "{sign}{hours:02}:{mins:02}:{secs:02}.{ms:03}")
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: Time,
    pub end: Time,
}

impl TimeRange {
    pub fn new(start: Time, end: Time) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn duration(self) -> Time {
        self.end.saturating_sub(self.start)
    }

    pub fn contains(self, t: Time) -> bool {
        self.start <= t && t < self.end
    }

    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    pub fn intersect(self, other: Self) -> Option<Self> {
        Self::new(self.start.max(other.start), self.end.min(other.end))
    }
}

/// A rational frame rate, e.g. `30000/1001` for 29.97 fps.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    pub const FALLBACK: Self = Self { num: 30, den: 1 };

    pub fn new(num: u32, den: u32) -> Option<Self> {
        (num > 0 && den > 0).then_some(Self { num, den })
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (num, den) = text.trim().split_once('/')?;
        Self::new(num.trim().parse().ok()?, den.trim().parse().ok()?)
    }

    pub fn fps(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn frame_at(self, t: Time) -> i64 {
        // Round to nearest frame boundary, using i128 to avoid overflow.
        let scaled = t.0 as i128 * self.num as i128 + (self.den as i128 * 500_000);
        (scaled / (self.den as i128 * 1_000_000)) as i64
    }

    pub fn time_of_frame(self, frame: i64) -> Time {
        let micros = frame as i128 * self.den as i128 * 1_000_000 / self.num as i128;
        Time(micros.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    pub fn frame_duration(self) -> Time {
        Time((self.den as i128 * 1_000_000 / self.num as i128) as i64)
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::FALLBACK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_seconds_exactly() {
        assert_eq!(Time::parse_secs("1"), Some(Time(1_000_000)));
        assert_eq!(Time::parse_secs("1.5"), Some(Time(1_500_000)));
        assert_eq!(Time::parse_secs("0.000001"), Some(Time(1)));
        assert_eq!(Time::parse_secs("0.0000009"), Some(Time(0)));
        assert_eq!(Time::parse_secs("-0.25"), Some(Time(-250_000)));
        assert_eq!(Time::parse_secs("N/A"), None);
    }

    #[test]
    fn timecode_display() {
        assert_eq!(Time(3_723_456_000).to_string(), "01:02:03.456");
        assert_eq!(Time(-1_500_000).to_string(), "-00:00:01.500");
    }

    #[test]
    fn frame_rate_round_trip() {
        let ntsc = FrameRate::parse("30000/1001").unwrap();
        assert!((ntsc.fps() - 29.970_029).abs() < 1e-5);
        assert_eq!(ntsc.frame_at(Time(1_000_000)), 30);
        assert_eq!(ntsc.time_of_frame(30), Time(1_001_000));
    }

    #[test]
    fn range_intersection() {
        let a = TimeRange::new(Time(0), Time(10_000_000)).unwrap();
        let b = TimeRange::new(Time(5_000_000), Time(20_000_000)).unwrap();
        assert_eq!(
            a.intersect(b),
            TimeRange::new(Time(5_000_000), Time(10_000_000))
        );
    }
}
