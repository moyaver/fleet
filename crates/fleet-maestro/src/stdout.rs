// SPDX-License-Identifier: AGPL-3.0-only
//! Maestro's stdout on the suite path (evidence lane B; research 08 Q1).
//!
//! It prints one line per *finished* flow, then per-shard summaries, then a promotion box.
//! Everything Fleet doesn't use is classified as noise; lines it doesn't recognise at all are
//! counted so output drift is visible (`parser.unrecognized`).

use crate::log::split_shard;

/// What one stdout line means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// `[Passed] <name> (<duration>)` or `[Failed] <name> (<duration>) (<message>)`.
    FlowResult {
        /// Shard, when sharded.
        shard: Option<u32>,
        /// Flow name.
        name: String,
        /// Whether it passed.
        passed: bool,
        /// Maestro's duration.
        duration_ms: Option<u64>,
        /// Failure message (absent when a device was lost).
        message: Option<String>,
    },
    /// `N/M Flow(s) Failed` or `N/M Flows Passed in <duration>`.
    Summary {
        /// Shard, when sharded.
        shard: Option<u32>,
        /// N.
        count: u32,
        /// M.
        total: u32,
        /// Whether this is the passed form.
        all_passed: bool,
    },
    /// `Will split N flows across S shards …` / `Will run S shards, with all N flows in each shard`.
    ShardPlan {
        /// Flows.
        flows: u32,
        /// Shards.
        shards: u32,
        /// `--shard-all`.
        all: bool,
    },
    /// `Flow <name> failed and continueOnFailure is set to false, aborting running sequential Flows`.
    SequenceAborted {
        /// The flow that failed.
        name: String,
    },
    /// Lines Fleet knows and ignores: blanks, `Waiting for flows to complete...`, the promotion
    /// box, JVM warnings, the single-flow step echo.
    Noise,
    /// Anything else.
    Unrecognized,
}

/// Classifies one line (without its line ending; ANSI already stripped).
#[must_use]
pub fn classify(raw: &str) -> Line {
    // A terminal (or `script`) can echo `^D` and backspaces in front of the first line.
    let line = raw
        .trim_end_matches(['\r', '\n'])
        .trim_start_matches(['\u{4}', '\u{8}']);
    let line = line
        .strip_prefix("^D")
        .unwrap_or(line)
        .trim_start_matches('\u{8}');
    let (shard, rest) = split_shard(line.trim_start());
    let rest = rest.trim();
    if rest.is_empty() || is_noise(rest) {
        return Line::Noise;
    }
    if let Some(r) = flow_result(shard, rest) {
        return r;
    }
    if let Some(s) = summary(shard, rest) {
        return s;
    }
    if let Some(p) = shard_plan(rest) {
        return p;
    }
    if let Some(name) = rest.strip_prefix("Flow ").and_then(|r| {
        r.strip_suffix(
            " failed and continueOnFailure is set to false, aborting running sequential Flows",
        )
    }) {
        return Line::SequenceAborted {
            name: name.to_owned(),
        };
    }
    Line::Unrecognized
}

fn is_noise(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Waiting for flows to complete",
        "WARNING: ",
        "Running on ",
        "> Flow ",
        "Requested ",
        "Debug tests faster",
        "Run your flows on Maestro Cloud",
        "maestro cloud ",
        // `--verbose` echoes the log to stdout.
        "[TRACE]",
        "[DEBUG]",
        "[ INFO]",
        "[ WARN]",
        "[ERROR]",
    ];
    // The promotion box is drawn with box-drawing characters.
    s.starts_with(['│', '╭', '╰', '─'])
        || PREFIXES.iter().any(|p| s.starts_with(p))
        || [" COMPLETED", " FAILED", " SKIPPED", " WARNED", " RUNNING"]
            .iter()
            .any(|t| s.ends_with(t) && s.contains("..."))
}

fn flow_result(shard: Option<u32>, s: &str) -> Option<Line> {
    let (passed, rest) = if let Some(r) = s.strip_prefix("[Passed] ") {
        (true, r)
    } else {
        (false, s.strip_prefix("[Failed] ")?)
    };
    // `<name> (<dur>)` or `<name> (<dur>) (<message>)`; the name itself may contain parentheses.
    let (name, duration_ms, message) = split_result(rest)?;
    Some(Line::FlowResult {
        shard,
        name,
        passed,
        duration_ms,
        message,
    })
}

fn split_result(rest: &str) -> Option<(String, Option<u64>, Option<String>)> {
    let mut search = rest;
    let mut offset = 0;
    while let Some(i) = search.find(" (") {
        let at = offset + i;
        let after = rest.get(at + 2..)?;
        if let Some(close) = after.find(')')
            && let Some(ms) = after.get(..close).and_then(parse_duration)
        {
            let name = rest.get(..at)?.to_owned();
            let tail = after.get(close + 1..)?.trim();
            let message = tail
                .strip_prefix('(')
                .and_then(|t| t.strip_suffix(')'))
                .map(str::to_owned);
            return Some((name, Some(ms), message));
        }
        offset = at + 2;
        search = rest.get(offset..)?;
    }
    None
}

/// Removes ANSI escape sequences (CSI `ESC [ … letter` and two-byte `ESC x`).
#[must_use]
pub fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
        } else if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Parses Maestro durations: `10s`, `1m 5s`, `2m`, `500ms`, `1h 2m 3s`.
#[must_use]
pub fn parse_duration(s: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for part in s.split_whitespace() {
        let digits = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        let (n, unit) = part.split_at(digits);
        let n: u64 = n.parse().ok()?;
        let ms = match unit {
            "ms" => n,
            "s" => n.checked_mul(1000)?,
            "m" => n.checked_mul(60_000)?,
            "h" => n.checked_mul(3_600_000)?,
            _ => return None,
        };
        total = total.checked_add(ms)?;
        any = true;
    }
    any.then_some(total)
}

fn summary(shard: Option<u32>, s: &str) -> Option<Line> {
    let (ratio, rest) = s.split_once(' ')?;
    let (count, total) = ratio.split_once('/')?;
    let (count, total) = (count.parse().ok()?, total.parse().ok()?);
    let all_passed = if rest.starts_with("Flow Failed") || rest.starts_with("Flows Failed") {
        false
    } else if rest.starts_with("Flow Passed") || rest.starts_with("Flows Passed") {
        true
    } else {
        return None;
    };
    Some(Line::Summary {
        shard,
        count,
        total,
        all_passed,
    })
}

fn shard_plan(s: &str) -> Option<Line> {
    if let Some(r) = s.strip_prefix("Will split ") {
        let (flows, r) = r.split_once(" flows across ")?;
        let (shards, _) = r.split_once(" shards")?;
        return Some(Line::ShardPlan {
            flows: flows.parse().ok()?,
            shards: shards.parse().ok()?,
            all: false,
        });
    }
    let r = s.strip_prefix("Will run ")?;
    let (shards, r) = r.split_once(" shards, with all ")?;
    let (flows, _) = r.split_once(" flows in each shard")?;
    Some(Line::ShardPlan {
        flows: flows.parse().ok()?,
        shards: shards.parse().ok()?,
        all: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_results() {
        assert_eq!(
            classify(
                "[shard 1] [Failed] delta-fails (19s) (Assertion is false: \"Fleet never shows this\" is visible)"
            ),
            Line::FlowResult {
                shard: Some(1),
                name: "delta-fails".into(),
                passed: false,
                duration_ms: Some(19_000),
                message: Some("Assertion is false: \"Fleet never shows this\" is visible".into()),
            }
        );
        assert_eq!(
            classify("[Failed] slow (32s)"),
            Line::FlowResult {
                shard: None,
                name: "slow".into(),
                passed: false,
                duration_ms: Some(32_000),
                message: None
            }
        );
        assert!(
            matches!(classify("[Passed] login (with otp) (1m 5s)"), Line::FlowResult { ref name, duration_ms: Some(65_000), .. } if name == "login (with otp)")
        );
    }

    #[test]
    fn summaries_plans_and_aborts() {
        assert_eq!(
            classify("1/4 Flow Failed"),
            Line::Summary {
                shard: None,
                count: 1,
                total: 4,
                all_passed: false
            }
        );
        assert_eq!(
            classify("[shard 2] 2/2 Flows Passed in 22s"),
            Line::Summary {
                shard: Some(2),
                count: 2,
                total: 2,
                all_passed: true
            }
        );
        assert_eq!(
            classify("Will split 4 flows across 2 shards (2 flows per shard)"),
            Line::ShardPlan {
                flows: 4,
                shards: 2,
                all: false
            }
        );
        assert_eq!(
            classify("Will run 2 shards, with all 4 flows in each shard"),
            Line::ShardPlan {
                flows: 4,
                shards: 2,
                all: true
            }
        );
        assert_eq!(
            classify(
                "Flow first-fails failed and continueOnFailure is set to false, aborting running sequential Flows"
            ),
            Line::SequenceAborted {
                name: "first-fails".into()
            }
        );
    }

    #[test]
    fn noise_and_drift() {
        for l in [
            "^D\u{8}\u{8}WARNING: x",
            "[ INFO] Debug output path: x",
            "",
            "Waiting for flows to complete...",
            "│   maestro cloud app_file flows_folder/   │",
            "WARNING: Use --enable-final-field-mutation",
            "Tap on \"Apps\"... COMPLETED",
            "\u{4}\u{8}\u{8}",
        ] {
            assert_eq!(classify(l), Line::Noise, "{l:?}");
        }
        assert_eq!(
            classify("Something Maestro 3 might print"),
            Line::Unrecognized
        );
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\u{1b}[36mWaiting\u{1b}[m done"), "Waiting done");
        assert!(matches!(strip_ansi("plain"), std::borrow::Cow::Borrowed(_)));
        assert_eq!(strip_ansi("trailing \u{1b}["), "trailing ");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h 2m 3s"), Some(3_723_000));
        assert_eq!(parse_duration("500ms"), Some(500));
        assert_eq!(parse_duration("2m"), Some(120_000));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration(""), None);
    }
}
