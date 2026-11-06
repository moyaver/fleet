// SPDX-License-Identifier: AGPL-3.0-only
//! `journal.jsonl`: the run's events on disk, one per line, capped (issue #37, I10).
//!
//! Each line is an [`EventEnvelope`] serialized as the outbox sends it, so the `seq` numbers in the
//! file are the ones the spool, the wire and the server carry. The file is bounded: once the next
//! line would take it past its cap, or a write to it fails (disk full, I/O error), the writer
//! stops, writes one [`Truncation`] marker in that line's place, and writes nothing else. The events
//! still reach the outbox and the server, and their numbering is unchanged, so `journal.gap` ranges
//! stay valid.
//!
//! The marker is a local record, not a protocol event. Journal lines are protocol events, but the
//! marker is never sent, so it has no `EventBody` and no place in the schema. It has no `seq` and
//! its `type` is not an event type, which is how [`parse`] tells it apart from an event.

use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

use fleet_protocol::journal::EventEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The cap when none is configured: 256 MiB, four times the spool.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// The `type` of the marker line. Deliberately not an event type.
const MARKER: &str = "journal.truncated";

/// The reason the writer stops when the cap is reached.
pub(crate) const REASON_BUDGET: &str = "budget";

/// Why the journal stopped, and what it holds before that point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    /// The first produced `seq` the journal does not hold. Every lower `seq` is in it.
    pub first_missing_seq: u64,
    /// Bytes of event lines written before the marker.
    pub bytes_written: u64,
    /// Why it stopped: `budget` at the cap, or `io: <kind>` for a failed write.
    pub reason: String,
}

/// The marker line: a `type`, then the [`Truncation`] fields.
#[derive(Serialize)]
struct MarkerLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    truncation: &'a Truncation,
}

/// The journal file as it is being written. Each line goes straight to `out`, with no buffer in
/// between, so after a failed write the file holds exactly the events before the one that failed.
#[derive(Debug)]
pub(crate) struct Writer<W: Write = File> {
    out: W,
    max_bytes: u64,
    /// Bytes of event lines written so far (the marker is not counted).
    written: u64,
    truncated: Option<Truncation>,
}

impl Writer {
    /// Creates `path`, replacing any earlier file. Event lines stop before passing `max_bytes`.
    pub(crate) fn create(path: &Path, max_bytes: u64) -> io::Result<Self> {
        Ok(Self::new(File::create(path)?, max_bytes))
    }
}

impl<W: Write> Writer<W> {
    fn new(out: W, max_bytes: u64) -> Self {
        Self {
            out,
            max_bytes,
            written: 0,
            truncated: None,
        }
    }

    /// Writes `event` as one line. If the line would take the file past its cap, or its write
    /// fails, writes the marker instead and stops: from then on nothing else is written.
    pub(crate) fn append(&mut self, event: &EventEnvelope) {
        if self.truncated.is_some() {
            return;
        }
        // An event that cannot be serialized is not written, as before this cap existed.
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let len = u64::try_from(line.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if self.written.saturating_add(len) > self.max_bytes {
            self.stop(event.seq, REASON_BUDGET.to_owned());
        } else if let Err(e) = writeln!(self.out, "{line}") {
            // The line may be part-written. Ending it here puts the marker on a line of its own,
            // so `parse` skips the torn start and still reads the marker.
            let _ = writeln!(self.out);
            self.stop(event.seq, format!("io: {:?}", e.kind()));
        } else {
            self.written += len;
        }
    }

    /// Writes the marker and remembers where the journal stopped. If the marker cannot be written
    /// (the disk is still failing), the truncation is recorded all the same.
    fn stop(&mut self, first_missing_seq: u64, reason: String) {
        let truncation = Truncation {
            first_missing_seq,
            bytes_written: self.written,
            reason,
        };
        let marker = MarkerLine {
            kind: MARKER,
            truncation: &truncation,
        };
        if let Ok(line) = serde_json::to_string(&marker) {
            let _ = writeln!(self.out, "{line}");
        }
        self.truncated = Some(truncation);
    }

    /// Where the journal stopped, once it has.
    pub(crate) const fn truncation(&self) -> Option<&Truncation> {
        self.truncated.as_ref()
    }
}

/// The contents of a journal file.
#[derive(Debug, Default)]
pub struct Journal {
    /// The events, in file order, up to the marker.
    pub events: Vec<EventEnvelope>,
    /// Set when the journal stopped early: at its cap, or on a failed write.
    pub truncated: Option<Truncation>,
}

/// Reads a journal's text: its events, up to the marker if there is one.
///
/// Lines that are neither an event nor the marker are skipped, so a torn last line (a crash while
/// writing) costs that line and nothing else.
#[must_use]
pub fn parse(text: &str) -> Journal {
    let mut journal = Journal::default();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["type"] == MARKER {
            journal.truncated = serde_json::from_value(value).ok();
            break;
        }
        if let Ok(event) = serde_json::from_value(value) {
            journal.events.push(event);
        }
    }
    journal
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{
        event::{EventBody, RunSignal},
        journal::{ClockSample, Provenance, SourceKind, Subject},
    };
    use time::OffsetDateTime;

    use super::*;

    fn event(seq: u64) -> EventEnvelope {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: seq,
        };
        EventEnvelope::new(
            seq,
            at,
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::RunSignal(RunSignal { signal: 2 }),
        )
    }

    /// A fresh directory for one test's journal; the test removes it when it is done.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fleet-journal-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn under_the_cap_the_file_is_the_events_one_per_line_and_nothing_else() {
        let dir = scratch("under");
        let path = dir.join("journal.jsonl");
        let mut w = Writer::create(&path, DEFAULT_MAX_BYTES).unwrap();
        for seq in 1..=3 {
            w.append(&event(seq));
        }
        let expected = (1..=3).fold(String::new(), |mut text, seq| {
            text.push_str(&serde_json::to_string(&event(seq)).unwrap());
            text.push('\n');
            text
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert!(w.truncation().is_none());
        let parsed = parse(&expected);
        assert_eq!(parsed.events.len(), 3);
        assert!(parsed.truncated.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn past_the_cap_it_stops_and_writes_one_marker_in_place_of_the_next_line() {
        let dir = scratch("capped");
        let path = dir.join("journal.jsonl");
        let line = serde_json::to_string(&event(1)).unwrap();
        // Room for two event lines, not a third.
        let cap = 2 * (line.len() as u64 + 1) + 4;
        let mut w = Writer::create(&path, cap).unwrap();
        for seq in 1..=5 {
            w.append(&event(seq));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        let marker: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(marker["type"], MARKER);
        assert_eq!(marker["first_missing_seq"], 3);
        assert_eq!(marker["bytes_written"], 2 * (line.len() as u64 + 1));
        assert_eq!(marker["reason"], REASON_BUDGET);
        assert!(
            text.len() as u64 <= cap + lines[2].len() as u64 + 1,
            "the file is at most the cap plus one marker line"
        );
        let parsed = parse(&text);
        assert_eq!(
            parsed.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            parsed.truncated,
            Some(Truncation {
                first_missing_seq: 3,
                bytes_written: 2 * (line.len() as u64 + 1),
                reason: REASON_BUDGET.into(),
            })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_torn_last_line_is_skipped() {
        let mut text = format!("{}\n", serde_json::to_string(&event(1)).unwrap());
        text.push_str("{\"seq\":2,\"ty");
        let parsed = parse(&text);
        assert_eq!(parsed.events.len(), 1);
        assert!(parsed.truncated.is_none());
    }

    /// A disk with `room` bytes, that fails once when it is full (as a full disk does) and then
    /// takes writes again, so a marker written after the failure is kept.
    #[derive(Debug)]
    struct FullDisk {
        room: usize,
        failed: bool,
        taken: Vec<u8>,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let room = self.room.saturating_sub(self.taken.len());
            if room == 0 && !self.failed {
                self.failed = true;
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            let n = if self.failed {
                buf.len()
            } else {
                room.min(buf.len())
            };
            self.taken.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_error_stops_the_journal_and_names_the_first_event_it_lost() {
        let line = serde_json::to_string(&event(1)).unwrap();
        let two_lines = 2 * (line.len() + 1);
        // Lines 1 and 2 fit, and the first 10 bytes of line 3 before the disk is full.
        let mut w = Writer::new(
            FullDisk {
                room: two_lines + 10,
                failed: false,
                taken: Vec::new(),
            },
            DEFAULT_MAX_BYTES,
        );
        for seq in 1..=5 {
            w.append(&event(seq));
        }
        let expected = Truncation {
            first_missing_seq: 3,
            bytes_written: two_lines as u64,
            reason: "io: StorageFull".into(),
        };
        assert_eq!(w.truncation(), Some(&expected));
        // The file: lines 1 and 2, the torn start of line 3, then the marker. Nothing after that.
        let text = String::from_utf8(w.out.taken.clone()).unwrap();
        let parsed = parse(&text);
        assert_eq!(
            parsed.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2],
            "{text}"
        );
        assert_eq!(parsed.truncated, Some(expected), "{text}");
        assert_eq!(text.lines().count(), 4, "{text}");
    }
}
