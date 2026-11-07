// SPDX-License-Identifier: AGPL-3.0-only
//! The device logcat excerpt of a failed flow (issue #75). Maestro writes `logs/device-logcat.txt`
//! for every flow: 0.5 MB when it passes, 4–7 MB when it fails. Fleet keeps a bounded part of it.
//!
//! Sans-I/O, like the other parsers: the agent opens the file and feeds it line by line, so the
//! log is never held in memory whole (I10). Three steps:
//!
//! 1. [`stamp`] reads a line's `MM-DD HH:MM:SS.mmm` prefix (`logcat -v time`). It has no year and
//!    no zone: it is the device's clock.
//! 2. [`align`] relates that clock to UTC. Maestro dumps the log right after the failing step ends,
//!    so the log's last line is the failure's end, give or take seconds. The difference, rounded
//!    to a quarter hour, is the device's zone. A log whose last line is not near the failure is
//!    refused, not guessed at.
//! 3. [`Selection`] keeps the lines of the failing step's [`Window`], and at most `cap` bytes of
//!    them: the newest, since the failure is at the end. It says how many older lines it cut.

use std::{collections::VecDeque, fmt::Write as _};

/// How long before the failing step starts the window begins.
pub const LEAD_MS: i64 = 10_000;
/// How long after the failing step ends the window ends.
pub const TAIL_MS: i64 = 2_000;
/// The most bytes an excerpt holds, marker included. The agent's cap and the server's default for
/// the `device_logcat` class.
pub const CAP_BYTES: usize = 512 * 1024;
/// Room kept for the truncation marker, so that an excerpt stays within its cap.
const MARKER_RESERVE: usize = 256;
/// The device's clock may be this far from the failure's end and still be aligned.
const MAX_RESIDUAL_MS: i64 = 60_000;
const QUARTER_HOUR_MS: i64 = 15 * 60_000;
const DAY_MS: i64 = 86_400_000;
/// A mean Gregorian year in milliseconds (365.2425 days).
const YEAR_MS: i64 = 31_556_952_000;

/// A failed step, in UTC milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// When the step started.
    pub start_ms: i64,
    /// When it failed.
    pub end_ms: i64,
}

impl Failure {
    /// The span of the log to keep: [`LEAD_MS`] before the step and [`TAIL_MS`] after it.
    #[must_use]
    pub fn window(self) -> Window {
        Window {
            from_ms: self.start_ms - LEAD_MS,
            to_ms: self.end_ms + TAIL_MS,
        }
    }
}

/// A span of UTC time, both ends included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first millisecond kept.
    pub from_ms: i64,
    /// The last millisecond kept.
    pub to_ms: i64,
}

/// The `MM-DD HH:MM:SS.mmm` prefix of a logcat line: the device's clock, without year or zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
}

/// The stamp at the start of `line`, or `None` for a line without one (the `---------
/// beginning of` headers, for example).
#[must_use]
pub fn stamp(line: &[u8]) -> Option<Stamp> {
    let p = line.get(..18)?;
    let sep = |i: usize, c: u8| (p.get(i) == Some(&c)).then_some(());
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = p.get(r)?;
        if !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    sep(2, b'-')?;
    sep(5, b' ')?;
    sep(8, b':')?;
    sep(11, b':')?;
    sep(14, b'.')?;
    Some(Stamp {
        month: num(0..2)?,
        day: num(3..5)?,
        hour: num(6..8)?,
        minute: num(9..11)?,
        second: num(12..14)?,
        millis: num(15..18)?,
    })
}

/// Days from 1970-01-01 to `y-m-d` in the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The stamp read as a date-time in `year`, in milliseconds since the epoch as if it were UTC.
/// `None` for a stamp that is not a date (a month of 13, for example).
fn naive_ms(year: i64, s: Stamp) -> Option<i64> {
    let valid = (1..=12).contains(&s.month)
        && (1..=31).contains(&s.day)
        && s.hour < 24
        && s.minute < 60
        && s.second < 60
        && s.millis < 1000;
    valid.then(|| {
        let seconds = s.hour * 3600 + s.minute * 60 + s.second;
        days_from_civil(year, s.month, s.day) * DAY_MS + seconds * 1000 + s.millis
    })
}

/// The year in which `s` falls nearest to `near_ms`. A stamp has no year, and a log spans far
/// less than a year, so the nearest one is the right one even across New Year.
fn nearest_year(s: Stamp, near_ms: i64) -> Option<i64> {
    let around = 1970 + near_ms.div_euclid(YEAR_MS);
    (around - 1..=around + 1)
        .filter_map(|y| naive_ms(y, s).map(|t| (y, (t - near_ms).abs())))
        .min_by_key(|(_, distance)| *distance)
        .map(|(y, _)| y)
}

/// How the device's clock relates to UTC: device time is UTC plus `offset_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    offset_ms: i64,
}

/// Relates the device's clock to UTC, from the log's last stamped line and the failure's end.
/// `None` when that line is more than a minute from the failure's end: the device's clock cannot
/// be told from the flow's, so no excerpt is cut.
#[must_use]
pub fn align(last: Stamp, failure_end_ms: i64) -> Option<Clock> {
    let local = naive_ms(nearest_year(last, failure_end_ms)?, last)?;
    let raw = local - failure_end_ms;
    let offset_ms = (raw + QUARTER_HOUR_MS / 2).div_euclid(QUARTER_HOUR_MS) * QUARTER_HOUR_MS;
    ((raw - offset_ms).abs() <= MAX_RESIDUAL_MS).then_some(Clock { offset_ms })
}

/// A line that is not UTF-8 text, or holds a NUL byte. It cannot be redacted reliably, so it
/// does not leave the runner (I6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotText;

/// The excerpt of a log: the text to upload and what it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    /// The lines kept, newest last, with a marker first when lines were cut.
    pub text: String,
    /// Lines of the window that the excerpt keeps.
    pub kept: u64,
    /// Lines of the window that the cap cut, oldest first.
    pub cut: u64,
}

/// Keeps the lines of a [`Window`] within a byte cap. Feed every line of the log in order.
#[derive(Debug)]
pub struct Selection {
    clock: Clock,
    window: Window,
    budget: usize,
    kept: VecDeque<String>,
    bytes: usize,
    cut: u64,
    /// Whether the last stamped line was inside the window. An unstamped line belongs with the
    /// line before it.
    inside: bool,
}

impl Selection {
    /// A selection that keeps at most `cap` bytes, marker included (never less than the marker
    /// needs).
    #[must_use]
    pub fn new(clock: Clock, window: Window, cap: usize) -> Self {
        Self {
            clock,
            window,
            budget: cap.saturating_sub(MARKER_RESERVE),
            kept: VecDeque::new(),
            bytes: 0,
            cut: 0,
            inside: false,
        }
    }

    /// Takes one line, without its newline. `redact` is applied to a line that is kept, so the
    /// cap counts what leaves. A line in the window that is not text is an error.
    ///
    /// # Errors
    ///
    /// [`NotText`] for a kept line that is not UTF-8 or holds a NUL byte.
    pub fn push(
        &mut self,
        line: &[u8],
        redact: impl FnOnce(&str) -> String,
    ) -> Result<(), NotText> {
        if let Some(inside) = stamp(line).and_then(|s| self.inside_at(s)) {
            self.inside = inside;
        }
        if !self.inside {
            return Ok(());
        }
        let text = std::str::from_utf8(line).map_err(|_| NotText)?;
        if text.contains('\0') {
            return Err(NotText);
        }
        let line = redact(text);
        self.bytes += line.len() + 1;
        self.kept.push_back(line);
        while self.bytes > self.budget {
            let Some(old) = self.kept.pop_front() else {
                break;
            };
            self.bytes -= old.len() + 1;
            self.cut += 1;
        }
        Ok(())
    }

    /// Whether a line stamped `s` is in the window, or `None` when `s` is not a date.
    fn inside_at(&self, s: Stamp) -> Option<bool> {
        let near = self.window.to_ms + self.clock.offset_ms;
        let local = naive_ms(nearest_year(s, near)?, s)?;
        let utc = local - self.clock.offset_ms;
        Some((self.window.from_ms..=self.window.to_ms).contains(&utc))
    }

    /// The excerpt. A marker first says how many lines the cap cut, when it cut any.
    #[must_use]
    pub fn finish(self) -> Excerpt {
        let mut text = String::new();
        if self.cut > 0 {
            let _ = writeln!(
                text,
                "[truncated: {} older lines of this window were cut to keep the excerpt within {} bytes; the newest lines are kept]",
                self.cut, CAP_BYTES
            );
        }
        for line in &self.kept {
            text.push_str(line);
            text.push('\n');
        }
        Excerpt {
            text,
            kept: u64::try_from(self.kept.len()).unwrap_or(u64::MAX),
            cut: self.cut,
        }
    }
}

/// The stamp of the last line that has one: the log's end, for [`align`].
#[must_use]
pub fn last_stamp<'a>(lines: impl IntoIterator<Item = &'a [u8]>) -> Option<Stamp> {
    lines.into_iter().filter_map(stamp).last()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crash fixture: Settings is killed with `am crash` at 01:41:27 local (UTC+2), inside the
    /// assertion that then fails. Its log is the 32 KiB head and tail of the capture.
    const CRASH_LOG: &[u8] = include_bytes!(
        "../../../fixtures/maestro/2.12.1/crash/run/debug/crash/logs/device-logcat.txt"
    );
    /// The failing assertion of the crash flow: `commands.json` says it started at
    /// 1794008484564 ms and ran 18287 ms.
    const CRASH_START_MS: i64 = 1_794_008_484_564;
    const CRASH_DURATION_MS: i64 = 18_287;

    /// The fail fixture: the assertion started at 1793751912442 ms and ran 15635 ms. The log is
    /// the head and tail of the capture, with the middle cut by the import.
    const FAIL_LOG: &[u8] = include_bytes!(
        "../../../fixtures/maestro/2.12.1/fail/run/debug/fail/logs/device-logcat.txt"
    );
    const FAIL_START_MS: i64 = 1_793_751_912_442;
    const FAIL_DURATION_MS: i64 = 15_635;

    const TWO_HOURS_MS: i64 = 2 * 3_600_000;

    fn lines(log: &[u8]) -> Vec<&[u8]> {
        log.strip_suffix(b"\n")
            .unwrap_or(log)
            .split(|b| *b == b'\n')
            .collect()
    }

    /// The lines of `log` that fall in `window`, with the captures' zone (UTC+2) applied by hand.
    /// An unstamped line belongs with the line before it, as [`Selection`] reads them.
    fn kept_by_hand(log: &[u8], window: Window) -> Vec<&[u8]> {
        let mut inside = false;
        let mut kept = Vec::new();
        for line in lines(log) {
            if let Some(s) = stamp(line) {
                let year = nearest_year(s, window.to_ms + TWO_HOURS_MS).expect("a year");
                let utc = naive_ms(year, s).expect("a date") - TWO_HOURS_MS;
                inside = (window.from_ms..=window.to_ms).contains(&utc);
            }
            if inside {
                kept.push(line);
            }
        }
        kept
    }

    fn crash_failure() -> Failure {
        Failure {
            start_ms: CRASH_START_MS,
            end_ms: CRASH_START_MS + CRASH_DURATION_MS,
        }
    }

    fn fail_failure() -> Failure {
        Failure {
            start_ms: FAIL_START_MS,
            end_ms: FAIL_START_MS + FAIL_DURATION_MS,
        }
    }

    fn clock_of(log: &[u8], failure: Failure) -> Clock {
        let last = last_stamp(lines(log)).expect("the log has a stamped line");
        align(last, failure.end_ms).expect("the log's end is near the failure")
    }

    #[test]
    fn reads_the_device_clock_prefix_and_refuses_other_lines() {
        let s = stamp(b"11-07 01:41:27.425 E/AndroidRuntime(10110): x").expect("a stamp");
        assert_eq!(
            s,
            Stamp {
                month: 11,
                day: 7,
                hour: 1,
                minute: 41,
                second: 27,
                millis: 425
            }
        );
        assert_eq!(stamp(b"--------- beginning of main"), None);
        assert_eq!(
            stamp(b"11-07 01:41:27,425 I/x"),
            None,
            "the separator is a dot"
        );
        assert_eq!(stamp(b"1a-07 01:41:27.425 I/x"), None, "digits only");
        assert_eq!(stamp(b"11-07 01:41"), None, "too short");
    }

    #[test]
    fn the_clock_of_a_real_capture_is_utc_plus_two() {
        // Both captures were made on a host at UTC+2: the log's last line is about when the
        // failure ended, so the offset is the zone, rounded to a quarter hour.
        assert_eq!(clock_of(CRASH_LOG, crash_failure()).offset_ms, TWO_HOURS_MS);
        assert_eq!(clock_of(FAIL_LOG, fail_failure()).offset_ms, TWO_HOURS_MS);
    }

    #[test]
    fn a_log_that_ends_far_from_the_failure_is_not_aligned() {
        let last = stamp(b"11-07 01:46:44.553 I/x").expect("a stamp");
        assert_eq!(
            align(last, crash_failure().end_ms),
            None,
            "five minutes after the failure is not the failure's end"
        );
        let near = stamp(b"11-07 01:41:44.553 I/x").expect("a stamp");
        assert!(align(near, crash_failure().end_ms).is_some());
    }

    #[test]
    fn the_window_keeps_the_lines_of_the_failing_step_and_no_others() {
        let failure = fail_failure();
        let window = failure.window();
        let clock = clock_of(FAIL_LOG, failure);
        let mut sel = Selection::new(clock, window, CAP_BYTES);
        for line in lines(FAIL_LOG) {
            sel.push(line, str::to_owned).expect("the log is text");
        }
        let excerpt = sel.finish();

        // An independent count: the lines whose device time falls in the window, converted by
        // hand with the zone the captures were made in.
        let expected = kept_by_hand(FAIL_LOG, window);
        assert!(!expected.is_empty(), "the window holds the failure");
        assert_eq!(excerpt.kept, expected.len() as u64);
        assert_eq!(excerpt.cut, 0, "a window this small fits the cap");
        assert!(
            !excerpt.text.contains("[truncated"),
            "no marker when nothing was cut"
        );
        // The failure itself is in it: the last kept line is the failing step's end, which Maestro
        // logged at 02:25:28 local, just before the last line of the capture.
        let last_kept = excerpt.text.lines().last().expect("kept lines");
        assert!(last_kept.starts_with("11-04 02:25:"), "{last_kept}");
    }

    #[test]
    fn the_cap_cuts_the_oldest_lines_and_says_so() {
        let failure = fail_failure();
        let clock = clock_of(FAIL_LOG, failure);
        let cap = 4096;
        let mut sel = Selection::new(clock, failure.window(), cap);
        for line in lines(FAIL_LOG) {
            sel.push(line, str::to_owned).expect("the log is text");
        }
        let excerpt = sel.finish();
        assert!(excerpt.cut > 0, "4 KiB is less than the window");
        assert!(
            excerpt.text.len() <= cap,
            "{} bytes, cap {cap}",
            excerpt.text.len()
        );
        let first = excerpt.text.lines().next().expect("a marker");
        assert!(first.starts_with("[truncated: "), "{first}");
        assert!(
            first.contains(&format!("{} older lines", excerpt.cut)),
            "{first}"
        );
        // The newest lines are the ones kept: the last kept line is the window's last line.
        let kept_tail: Vec<&str> = excerpt.text.lines().skip(1).collect();
        assert_eq!(kept_tail.len() as u64, excerpt.kept);
        let last_in_log = *kept_by_hand(FAIL_LOG, failure.window())
            .last()
            .expect("a line in the window");
        assert_eq!(kept_tail.last().map(|l| l.as_bytes()), Some(last_in_log));
    }

    #[test]
    fn a_kept_line_is_redacted_and_counted_after_redaction() {
        let failure = crash_failure();
        let clock = clock_of(CRASH_LOG, failure);
        let mut sel = Selection::new(clock, failure.window(), CAP_BYTES);
        for line in lines(CRASH_LOG) {
            sel.push(line, |t| t.replace("Settings", "[REDACTED]"))
                .expect("the log is text");
        }
        let excerpt = sel.finish();
        assert!(excerpt.kept > 0);
        assert!(!excerpt.text.contains("Settings"), "the secret stays out");
        assert!(excerpt.text.contains("[REDACTED]"));
    }

    #[test]
    fn a_line_in_the_window_that_is_not_text_is_refused_and_one_outside_is_not_read() {
        let failure = crash_failure();
        let clock = clock_of(CRASH_LOG, failure);
        let inside = b"11-07 01:41:30.000 I/x: \xff bad";
        let outside = b"11-07 01:00:00.000 I/x: \xff bad";
        let mut sel = Selection::new(clock, failure.window(), CAP_BYTES);
        assert_eq!(sel.push(outside, str::to_owned), Ok(()));
        assert_eq!(sel.push(inside, str::to_owned), Err(NotText));
        let mut nul = Selection::new(clock, failure.window(), CAP_BYTES);
        assert_eq!(
            nul.push(b"11-07 01:41:30.000 I/x: a\0b", str::to_owned),
            Err(NotText)
        );
    }

    #[test]
    fn a_line_without_a_stamp_belongs_with_the_line_before_it() {
        let failure = crash_failure();
        let clock = clock_of(CRASH_LOG, failure);
        let mut sel = Selection::new(clock, failure.window(), CAP_BYTES);
        sel.push(b"11-07 01:41:30.000 E/x: first", str::to_owned)
            .expect("text");
        sel.push(b"\tcontinued", str::to_owned).expect("text");
        sel.push(b"11-07 01:00:00.000 E/x: outside", str::to_owned)
            .expect("text");
        sel.push(b"\tstill outside", str::to_owned).expect("text");
        let excerpt = sel.finish();
        assert_eq!(excerpt.text, "11-07 01:41:30.000 E/x: first\n\tcontinued\n");
        assert_eq!(excerpt.kept, 2);
    }
}
