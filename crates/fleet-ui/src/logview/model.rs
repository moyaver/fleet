// SPDX-License-Identifier: AGPL-3.0-only
//! The log model: lines with styled spans, case-insensitive search, anchors, step jumps.

use crate::logview::ansi::{self, Span};

/// One log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Styled spans of the line.
    pub spans: Vec<Span>,
    /// The text with all styling removed; what search runs over.
    pub plain: String,
    /// Byte offset of the line start in the original log text.
    pub offset: usize,
}

/// A parsed log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogModel {
    lines: Vec<Line>,
    len: usize,
}

/// A search hit: byte range `start..end` into [`Line::plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Zero-based line index.
    pub line: usize,
    /// Start byte in the plain text (a char boundary).
    pub start: usize,
    /// End byte in the plain text (a char boundary).
    pub end: usize,
}

/// Where a step's output begins in the log, as a byte offset into the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepStart {
    /// The step's id.
    pub step: String,
    /// Byte offset of the first byte the step logged.
    pub offset: usize,
}

impl LogModel {
    /// Parses `text`, splitting on `\n` (a trailing newline adds no empty line).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut offset = 0;
        for raw in text.split_inclusive('\n') {
            let content = raw.strip_suffix('\n').unwrap_or(raw);
            let spans = ansi::parse(content);
            let plain = spans.iter().map(|s| s.text.as_str()).collect();
            lines.push(Line {
                spans,
                plain,
                offset,
            });
            offset += raw.len();
        }
        Self {
            lines,
            len: text.len(),
        }
    }

    /// The lines.
    #[must_use]
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// The line at a zero-based index.
    #[must_use]
    pub fn line(&self, index: usize) -> Option<&Line> {
        self.lines.get(index)
    }

    /// The zero-based line holding byte `offset`, or `None` past the end of the log.
    #[must_use]
    pub fn line_at_offset(&self, offset: usize) -> Option<usize> {
        if offset >= self.len {
            return None;
        }
        Some(
            self.lines
                .partition_point(|l| l.offset <= offset)
                .saturating_sub(1),
        )
    }

    /// The line to scroll to for `step`, given where each step starts.
    #[must_use]
    pub fn jump_to_step(&self, starts: &[StepStart], step: &str) -> Option<usize> {
        let start = starts.iter().find(|s| s.step == step)?;
        self.line_at_offset(start.offset)
    }

    /// Finds every case-insensitive, non-overlapping occurrence of `query`.
    #[must_use]
    pub fn search(&self, query: &str) -> Search {
        let needle: Vec<char> = query.chars().map(fold).collect();
        let mut matches = Vec::new();
        if !needle.is_empty() {
            for (line, l) in self.lines.iter().enumerate() {
                let hay: Vec<(usize, char)> =
                    l.plain.char_indices().map(|(i, c)| (i, fold(c))).collect();
                let mut i = 0;
                while i + needle.len() <= hay.len() {
                    let hit = hay[i..i + needle.len()]
                        .iter()
                        .map(|(_, c)| c)
                        .eq(needle.iter());
                    if hit {
                        let end = hay.get(i + needle.len()).map_or(l.plain.len(), |(b, _)| *b);
                        matches.push(Match {
                            line,
                            start: hay[i].0,
                            end,
                        });
                        i += needle.len();
                    } else {
                        i += 1;
                    }
                }
            }
        }
        Search {
            matches,
            current: None,
        }
    }
}

/// Simple case folding that keeps one char per char, so byte offsets map back.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// A search result with a cursor over the matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    matches: Vec<Match>,
    current: Option<usize>,
}

impl Search {
    /// All matches in log order.
    #[must_use]
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// The number of matches.
    #[must_use]
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The selected match.
    #[must_use]
    pub fn current(&self) -> Option<Match> {
        self.current.and_then(|i| self.matches.get(i)).copied()
    }

    /// One-based position of the selected match, for "3 of 17".
    #[must_use]
    pub fn position(&self) -> Option<usize> {
        self.current.map(|i| i + 1)
    }

    /// Selects the next match, wrapping to the first.
    pub fn select_next(&mut self) -> Option<Match> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = Some(self.current.map_or(0, |i| (i + 1) % self.matches.len()));
        self.current()
    }

    /// Selects the previous match, wrapping to the last.
    pub fn select_prev(&mut self) -> Option<Match> {
        let n = self.matches.len();
        if n == 0 {
            return None;
        }
        self.current = Some(self.current.map_or(n - 1, |i| (i + n - 1) % n));
        self.current()
    }

    /// Selects the first match at or after `line`, wrapping to the first match.
    pub fn select_from_line(&mut self, line: usize) -> Option<Match> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = Some(
            self.matches
                .iter()
                .position(|m| m.line >= line)
                .unwrap_or(0),
        );
        self.current()
    }
}

/// The `#L123` fragment for a zero-based line index.
#[must_use]
pub fn anchor(line: usize) -> String {
    format!("#L{}", line + 1)
}

/// The zero-based line a `#L123` fragment names (the `#` is optional); `None` if malformed.
#[must_use]
pub fn parse_anchor(fragment: &str) -> Option<usize> {
    let digits = fragment
        .strip_prefix('#')
        .unwrap_or(fragment)
        .strip_prefix('L')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()?.checked_sub(1)
}

/// The zero-based line a `#L123` fragment names in a log of `lines` lines; `None` if malformed
/// or past the end.
#[must_use]
pub fn anchored_line(fragment: &str, lines: usize) -> Option<usize> {
    parse_anchor(fragment).filter(|&line| line < lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "start\n\x1b[31mERROR\x1b[0m: Boom\nnext boom BOOM\n";

    #[test]
    fn lines_split_on_newline_and_search_sees_plain_text_not_escapes() {
        let log = LogModel::parse(LOG);
        assert_eq!(log.lines().len(), 3);
        assert_eq!(log.line(1).unwrap().plain, "ERROR: Boom");
        assert_eq!(log.search("[31m").count(), 0);
        assert_eq!(log.search("x1b").count(), 0);
    }

    #[test]
    fn search_is_case_insensitive_with_a_count_and_char_boundary_ranges() {
        let log = LogModel::parse(LOG);
        let s = log.search("boom");
        assert_eq!(s.count(), 3);
        let m = s.matches()[2];
        assert_eq!((m.line, m.start, m.end), (2, 10, 14));
        let uni = LogModel::parse("héllo ÉCOLE école\n");
        let hits = uni.search("école");
        assert_eq!(hits.count(), 2);
        let m = hits.matches()[0];
        assert_eq!(&uni.line(0).unwrap().plain[m.start..m.end], "ÉCOLE");
    }

    #[test]
    fn next_and_prev_wrap_and_an_empty_query_matches_nothing() {
        let log = LogModel::parse(LOG);
        let mut s = log.search("boom");
        assert_eq!(s.current(), None);
        assert_eq!(s.select_next().unwrap().line, 1);
        assert_eq!(s.position(), Some(1));
        s.select_next();
        s.select_next();
        assert_eq!(s.select_next().unwrap().line, 1, "wraps to the first");
        assert_eq!(s.select_prev().unwrap().line, 2, "wraps back to the last");
        let mut none = log.search("");
        assert_eq!(
            (none.count(), none.select_next(), none.select_prev()),
            (0, None, None)
        );
        assert_eq!(log.search("zzz").select_next(), None);
    }

    #[test]
    fn matches_do_not_overlap() {
        let log = LogModel::parse("aaaa\n");
        assert_eq!(log.search("aa").count(), 2);
    }

    #[test]
    fn select_from_line_starts_at_the_first_match_on_or_after_it() {
        let log = LogModel::parse(LOG);
        let mut s = log.search("boom");
        assert_eq!(s.select_from_line(2).unwrap().line, 2);
        assert_eq!(s.select_from_line(9).unwrap().line, 1, "wraps");
    }

    #[test]
    fn anchors_are_one_based_and_reject_garbage() {
        assert_eq!(anchor(122), "#L123");
        assert_eq!(parse_anchor("#L123"), Some(122));
        assert_eq!(parse_anchor("L1"), Some(0));
        for bad in [
            "",
            "#",
            "#L",
            "#L0",
            "#L-3",
            "#Lx",
            "#l5",
            "#L1a",
            "#L99999999999999999999999",
        ] {
            assert_eq!(parse_anchor(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_anchor_names_a_line_only_when_the_log_has_it() {
        assert_eq!(anchored_line("#L3", 3), Some(2));
        assert_eq!(anchored_line("#L4", 3), None, "past the end");
        assert_eq!(anchored_line("#L1", 0), None, "empty log");
        assert_eq!(anchored_line("", 3), None);
        assert_eq!(anchored_line("#top", 3), None);
    }

    #[test]
    fn a_step_start_offset_maps_to_the_line_that_contains_it() {
        let log = LogModel::parse(LOG);
        // "start\n" is 6 bytes, line 1 starts at 6; line 2 starts at 6 + 1+... raw.
        let second = LOG.find("next").unwrap();
        let starts = [
            StepStart {
                step: "a".into(),
                offset: 0,
            },
            StepStart {
                step: "b".into(),
                offset: 8,
            },
            StepStart {
                step: "c".into(),
                offset: second,
            },
            StepStart {
                step: "late".into(),
                offset: LOG.len(),
            },
        ];
        assert_eq!(log.jump_to_step(&starts, "a"), Some(0));
        assert_eq!(
            log.jump_to_step(&starts, "b"),
            Some(1),
            "mid-line offsets land on their line"
        );
        assert_eq!(log.jump_to_step(&starts, "c"), Some(2));
        assert_eq!(log.jump_to_step(&starts, "late"), None);
        assert_eq!(log.jump_to_step(&starts, "unknown"), None);
    }

    #[test]
    fn a_log_without_trailing_newline_keeps_its_last_line_and_empty_is_empty() {
        assert_eq!(LogModel::parse("a\nb").lines().len(), 2);
        assert_eq!(LogModel::parse("").lines().len(), 0);
        assert_eq!(LogModel::parse("a\n\nb\n").lines().len(), 3);
    }

    #[test]
    fn captured_stdout_becomes_lines_with_anchors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/maestro/2.12.1/verbose/stdout.bin"
        );
        let raw = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        let log = LogModel::parse(&raw);
        assert!(log.lines().len() > 3);
        assert!(log.lines().iter().all(|l| !l.plain.contains('\x1b')));
        let last = log.lines().len() - 1;
        assert_eq!(parse_anchor(&anchor(last)), Some(last));
    }
}
