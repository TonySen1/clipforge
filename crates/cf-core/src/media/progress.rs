//! Parsing `ffmpeg -progress pipe:1`.
//!
//! FFmpeg writes a flat `key=value` block per update and a final
//! `progress=end`. Blocks are separated by `progress=continue`, so a parser
//! that only looks at the newest lines can still lose a key if it is cut
//! across reads — hence the accumulating parser below.

use crate::time::Time;

/// One progress update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// Output timestamp reached so far.
    pub out_time: Option<Time>,
    /// Frames written, for streams that have them.
    pub frame: Option<u64>,
    /// Current encoding speed multiplier, e.g. 497 for `497x`.
    pub speed: Option<u32>,
    /// True when FFmpeg reported `progress=end`.
    pub finished: bool,
}

/// Incremental parser: feed it stderr/stdout chunks as they arrive.
#[derive(Debug, Default)]
pub struct ProgressParser {
    pending: String,
    /// Latest update emitted, so a block split across reads still completes.
    current: ProgressUpdate,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds raw bytes and returns any completed update.
    pub fn push(&mut self, chunk: &[u8]) -> Option<ProgressUpdate> {
        self.pending.push_str(&String::from_utf8_lossy(chunk));

        // Only whole lines are safe to interpret.
        let mut last_complete: Option<ProgressUpdate> = None;
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            if let Some(update) = self.consume_line(line.trim_end()) {
                last_complete = Some(update);
            }
        }

        last_complete
    }

    fn consume_line(&mut self, line: &str) -> Option<ProgressUpdate> {
        let (key, value) = line.split_once('=')?;
        let value = value.trim();

        match key {
            "out_time_us" | "out_time_ms" => {
                // Both keys carry microseconds (ffmpeg's `out_time_ms` is a
                // long-standing misnomer).
                if let Ok(micros) = value.parse::<i64>() {
                    if micros >= 0 {
                        self.current.out_time = Some(Time(micros));
                    }
                }
            }
            "out_time" => {
                // Fallback for builds that omit the numeric key. The value is
                // `HH:MM:SS.micro`, not decimal seconds.
                if self.current.out_time.is_none() {
                    self.current.out_time = parse_timecode(value);
                }
            }
            "frame" => {
                if let Ok(frames) = value.parse() {
                    self.current.frame = Some(frames);
                }
            }
            "speed" => {
                // Formats seen: "497x", " 1.2x", "N/A".
                let trimmed = value.trim_end_matches('x').trim();
                if let Ok(speed) = trimmed.parse::<f32>() {
                    if speed.is_finite() && speed >= 0.0 {
                        self.current.speed = Some(speed as u32);
                    }
                }
            }
            "progress" => {
                let finished = value == "end";
                let mut update = std::mem::take(&mut self.current);
                update.finished = finished;
                if finished {
                    // Keep the final values available to callers that only
                    // read `finished`, then reset for any subsequent job.
                    self.current = ProgressUpdate::default();
                }
                return Some(update);
            }
            _ => {}
        }

        None
    }
}

/// Parses `HH:MM:SS.micro`, the format `out_time` uses.
pub fn parse_timecode(text: &str) -> Option<Time> {
    let mut parts = text.trim().split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds_text = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let seconds = Time::parse_secs(seconds_text)?;
    Some(Time(
        hours
            .checked_mul(3_600_000_000)?
            .checked_add(minutes.checked_mul(60_000_000)?)?
            .checked_add(seconds.micros())?,
    ))
}

/// Percentage of `total` reached, clamped to 0..=100.
pub fn percent(done: Time, total: Time) -> f32 {
    if total.micros() <= 0 {
        return 100.0;
    }
    let ratio = done.micros() as f64 / total.micros() as f64;
    (ratio * 100.0).clamp(0.0, 100.0) as f32
}

/// Keeps only the last `keep` non-empty lines of FFmpeg's stderr.
///
/// The full log is written to a file elsewhere; the UI gets a summary.
pub fn stderr_summary(stderr: &str, keep: usize) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(keep);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut ProgressParser, text: &str) -> Option<ProgressUpdate> {
        parser.push(text.as_bytes())
    }

    #[test]
    fn parses_a_complete_block() {
        let mut parser = ProgressParser::new();
        let update = feed(
            &mut parser,
            "frame=62\nfps=0.00\nout_time_us=2066667\nspeed= 497x\nprogress=continue\n",
        )
        .unwrap();

        assert_eq!(update.out_time, Some(Time(2_066_667)));
        assert_eq!(update.frame, Some(62));
        assert_eq!(update.speed, Some(497));
        assert!(!update.finished);
    }

    #[test]
    fn marks_the_final_block() {
        let mut parser = ProgressParser::new();
        feed(&mut parser, "out_time_us=1000000\nprogress=continue\n");
        let last = feed(&mut parser, "out_time_us=2000000\nprogress=end\n").unwrap();

        assert!(last.finished);
        assert_eq!(last.out_time, Some(Time(2_000_000)));
    }

    #[test]
    fn reassembles_a_block_split_across_reads() {
        let mut parser = ProgressParser::new();
        assert!(feed(&mut parser, "out_time_us=1").is_none());
        assert!(feed(&mut parser, "500000\nprog").is_none());
        let update = feed(&mut parser, "ress=end\n").unwrap();
        assert_eq!(update.out_time, Some(Time(1_500_000)));
        assert!(update.finished);
    }

    #[test]
    fn tolerates_garbage_and_missing_keys() {
        let mut parser = ProgressParser::new();
        let update = feed(&mut parser, "not a key value pair\nprogress=continue\n").unwrap();
        assert_eq!(update.out_time, None);
        assert!(!update.finished);
    }

    #[test]
    fn ignores_negative_timestamps() {
        let mut parser = ProgressParser::new();
        let update = feed(&mut parser, "out_time_us=-5000\nprogress=continue\n").unwrap();
        assert_eq!(update.out_time, None);
    }

    #[test]
    fn falls_back_to_the_text_timestamp() {
        let mut parser = ProgressParser::new();
        let update = feed(&mut parser, "out_time=00:00:02.066667\nprogress=continue\n").unwrap();
        assert_eq!(update.out_time, Some(Time(2_066_667)));
    }

    #[test]
    fn numeric_key_wins_over_text() {
        let mut parser = ProgressParser::new();
        let update = feed(
            &mut parser,
            "out_time_us=123456\nout_time=00:00:09.000000\nprogress=continue\n",
        )
        .unwrap();
        assert_eq!(update.out_time, Some(Time(123_456)));
    }

    #[test]
    fn parses_hh_mm_ss_timecodes() {
        assert_eq!(parse_timecode("00:00:02.066667"), Some(Time(2_066_667)));
        assert_eq!(parse_timecode("01:02:03.500000"), Some(Time(3_723_500_000)));
        assert_eq!(parse_timecode("garbage"), None);
        assert_eq!(parse_timecode("1:2"), None);
    }

    #[test]
    fn speed_na_is_ignored() {
        let mut parser = ProgressParser::new();
        let update = feed(&mut parser, "speed=N/A\nprogress=continue\n").unwrap();
        assert_eq!(update.speed, None);
    }

    #[test]
    fn percent_is_clamped() {
        let total = Time::from_secs(10.0);
        assert_eq!(percent(Time::ZERO, total), 0.0);
        assert_eq!(percent(Time::from_secs(5.0), total), 50.0);
        assert_eq!(percent(Time::from_secs(20.0), total), 100.0);
    }

    #[test]
    fn percent_of_zero_duration_is_complete() {
        assert_eq!(percent(Time::ZERO, Time::ZERO), 100.0);
    }

    #[test]
    fn stderr_summary_keeps_the_tail() {
        let log = "line1\nline2\n\nline3\nline4\n";
        assert_eq!(stderr_summary(log, 2), "line3\nline4");
        assert_eq!(stderr_summary("short", 5), "short");
    }
}
