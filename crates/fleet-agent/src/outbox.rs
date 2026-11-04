// SPDX-License-Identifier: AGPL-3.0-only
//! The outbox: journal events waiting for the server's ack (research 02 §7, research 05 §3.3;
//! invariants I5 and I10, ADR-0005 and its 2026-11-04 amendment, PLAN §6.6).
//!
//! # Shape
//!
//! ```text
//!   observer ──push──▶ spool file (source of truth, capped) ──refill──▶ send window ──▶ transport
//!                                                         ◀──────── acks ──────────────┘
//! ```
//!
//! * The **spool** ([`spool`]) holds every stored event until the server has acked it, up to
//!   [`Budget::spool_bytes`]. When everything in it is acked the file starts over, so a healthy
//!   connection never runs a long suite out of space.
//! * The **send window** is a small in-memory view of the next unacked events: at most
//!   [`Budget::window_events`] events and [`Budget::window_bytes`] bytes (plus a small reserve for
//!   the events that must never be refused). It is read back from the spool as it drains, so memory
//!   does not depend on how many events are waiting. Without a spool (no run directory, or the
//!   file can't be written) the window is the only store and holds what fits.
//! * **Degradation is explicit.** An event that fits nowhere is not stored. The outbox keeps one
//!   pending gap, and before the next event it stores (or when the run is sealed) it stores a
//!   `journal.gap` marker, so the loss is on the record.
//!
//! # One sequence space
//!
//! A `seq` is assigned once, by the session, and means the same thing everywhere: in
//! `journal.jsonl`, the spool, on the wire and in the server's tables. The outbox never
//! renumbers. A dropped event keeps its `seq` and leaves a **hole**; the marker that reports the
//! hole is sequenced inside it, at its last slot: `seq == to_seq`, with `from_seq` the first
//! dropped `seq`. `from_seq..=to_seq` is the dropped events (the last one's slot is the marker's
//! own), and the journal is gap-free except for holes a marker covers (ADR-0005 amendment). The
//! server accepts a marker when `from_seq` is the next `seq` it expects and moves past the hole;
//! any other skip is still a `seq_gap`. Every dropped event is covered by exactly one marker.
//!
//! Because seqs skip, the window and the spool index work in seq order rather than by arithmetic:
//! the window remembers the range of seqs it covers completely, and the spool keeps a sparse
//! `(seq, offset)` index.
//!
//! # What is never refused
//!
//! `journal.gap` markers and `run.ended` use a reserve (spool bytes and window slots) that
//! ordinary events can't touch, so the record of a loss and the end of the run always fit. The
//! one case a marker can wait is a spool that can't be written while a backlog sits on disk: it
//! is then stored as soon as the window has caught up (it is retried on every ack once sealed).

mod spool;

use std::{collections::VecDeque, path::Path};

use fleet_protocol::{
    event::{EventBody, JournalGap},
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
};
use uuid::Uuid;

use self::spool::{RECORD_OVERHEAD, Spool};

/// Event type that closes a run; it is stored from the reserve if need be.
const RUN_ENDED: &str = "run.ended";

/// The most a serialized `journal.gap` envelope can take. The reserve always has this much left
/// for the gap record (checked by a test against the worst case).
const GAP_RECORD_MAX: u64 = 1024;

/// Most events read from the spool per refill, so the lock is never held for a long read.
const REFILL_EVENTS: usize = 128;

/// Memory and disk limits of an [`Outbox`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Spool file size cap in bytes (PLAN §6.6: 64 MiB).
    pub(crate) spool_bytes: u64,
    /// The top part of the spool only `journal.gap` and `run.ended` may use.
    pub(crate) spool_reserve: u64,
    /// The spool starts over once everything in it is acked and it has reached this size.
    pub(crate) rotate_bytes: u64,
    /// Events in the send window. Above the server's `max_inflight_events` (512 by default), so
    /// the window can hold a full flight.
    pub(crate) window_events: usize,
    /// Serialized bytes in the send window.
    pub(crate) window_bytes: usize,
    /// Extra window slots for `journal.gap` and `run.ended`.
    pub(crate) reserve_events: usize,
    /// Extra window bytes for `journal.gap` and `run.ended`.
    pub(crate) reserve_bytes: usize,
    /// An event whose JSON is larger than this is not stored (it becomes a gap). The server's
    /// frame limit is 1 MiB, so a larger event could not be sent anyway.
    pub(crate) max_event_bytes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            spool_bytes: 64 * 1024 * 1024,
            spool_reserve: 512 * 1024,
            rotate_bytes: 1024 * 1024,
            window_events: 1024,
            window_bytes: 2 * 1024 * 1024,
            reserve_events: 8,
            reserve_bytes: 512 * 1024,
            max_event_bytes: 256 * 1024,
        }
    }
}

/// How much room an event may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Everything else: only the ordinary budget.
    Normal,
    /// `run.ended`: the ordinary budget plus the reserve, minus room for one gap record.
    Final,
    /// `journal.gap`: the whole budget.
    Gap,
}

/// Why events were not stored; the `reason` of the `journal.gap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    SpoolFull,
    SpoolError,
    WindowFull,
    Oversize,
    Unserializable,
}

impl Reason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SpoolFull => "spool_full",
            Self::SpoolError => "spool_error",
            Self::WindowFull => "window_full",
            Self::Oversize => "event_too_large",
            Self::Unserializable => "unserializable",
        }
    }
}

/// A run of dropped events not yet reported.
#[derive(Debug, Clone)]
struct PendingGap {
    /// `seq` of the first dropped event.
    from: u64,
    /// `seq` of the last dropped event; the marker is stored in this slot.
    to: u64,
    reason: Reason,
    /// When the last of them happened.
    at: ClockSample,
}

impl PendingGap {
    /// The marker: sequenced at `to`, covering `from..=to`.
    fn event(&self) -> EventEnvelope {
        EventEnvelope::new(
            self.to,
            self.at,
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::JournalGap(JournalGap {
                from_seq: self.from,
                to_seq: self.to,
                reason: self.reason.as_str().into(),
            }),
        )
    }
}

/// An event in the window and its serialized size.
#[derive(Debug)]
struct Slot {
    event: EventEnvelope,
    bytes: usize,
}

/// Events waiting for an ack, in bounded memory.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    budget: Budget,
    /// Stored events in `seq` order. It holds *every* stored event with `seq` in
    /// `(covers_after, covered_through]` that isn't acked, so a reader can start anywhere in that
    /// range without missing one.
    window: VecDeque<Slot>,
    window_bytes: usize,
    covers_after: u64,
    covered_through: u64,
    acked_through: u64,
    /// Highest `seq` stored.
    last_seq: u64,
    /// Dropped events not yet reported by a `journal.gap`.
    gap: Option<PendingGap>,
    /// No more events will be pushed: a pending gap is stored as soon as it can be.
    sealed: bool,
    spool: Option<Spool>,
    /// Events dropped in total.
    dropped: u64,
}

impl Outbox {
    /// A memory-only outbox: the window is the only store.
    #[cfg(test)]
    pub(crate) fn memory(budget: Budget) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }

    /// An outbox that spools to `path`. If the file can't be created the outbox is memory-only:
    /// the run goes on and what doesn't fit the window is reported as a gap.
    pub(crate) fn with_spool(path: &Path, agent_run_id: Uuid, budget: Budget) -> Self {
        let spool = match Spool::create(path, agent_run_id) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "cannot create the spool; events stay in memory only");
                None
            }
        };
        Self {
            budget,
            spool,
            ..Self::default()
        }
    }

    /// An outbox over a spool left by a run whose agent died, for `fleet-agent flush`. Nothing is
    /// loaded: the window fills from the file as the transport sends.
    pub(crate) fn replay(path: &Path) -> std::io::Result<(Uuid, Self)> {
        let spool = Spool::open_replay(path)?;
        let id = spool.run_id();
        Ok((
            id,
            Self {
                acked_through: spool.first_seq() - 1,
                last_seq: spool.through(),
                sealed: true,
                spool: Some(spool),
                ..Self::default()
            },
        ))
    }

    /// Adds events in `seq` order. Events that fit nowhere are dropped and reported by a
    /// `journal.gap`; the call never fails and never grows memory past the budget.
    pub(crate) fn push(&mut self, events: Vec<EventEnvelope>) {
        for e in events {
            self.push_one(e);
        }
    }

    fn push_one(&mut self, e: EventEnvelope) {
        let (seq, at) = (e.seq, e.at);
        if seq < self.next_seq() {
            // The session numbers events upward; this would break the order everything relies on.
            tracing::error!(
                seq,
                last = self.next_seq() - 1,
                "event out of order; ignored"
            );
            return;
        }
        let class = if e.kind == RUN_ENDED {
            Class::Final
        } else {
            Class::Normal
        };
        let Some(bytes) = encode(&e) else {
            return self.drop_event(seq, at, Reason::Unserializable);
        };
        if bytes.len() > self.budget.max_event_bytes {
            return self.drop_event(seq, at, Reason::Oversize);
        }
        // An event that doesn't fit alone doesn't fit with a marker in front of it either: the
        // common case once the spool is full costs one serialization and no more.
        if let Err(reason) = self.admits(bytes.len(), 1, class) {
            return self.drop_event(seq, at, reason);
        }
        let marker = match self.gap.as_ref().map(PendingGap::event) {
            Some(g) => match encode(&g) {
                Some(b) => Some((g, b)),
                None => return self.drop_event(seq, at, Reason::Unserializable),
            },
            None => None,
        };
        let total = bytes.len() + marker.as_ref().map_or(0, |(_, b)| b.len());
        let count = 1 + usize::from(marker.is_some());
        if let Err(reason) = self.admits(total, count, class) {
            return self.drop_event(seq, at, reason);
        }
        if let Some((g, gb)) = marker {
            if !self.store(g, &gb, Class::Gap) {
                return self.drop_event(seq, at, Reason::SpoolError);
            }
            self.gap = None;
        }
        if !self.store(e, &bytes, class) {
            self.drop_event(seq, at, Reason::SpoolError);
        }
    }

    /// Records an event that will not be sent, extending the pending gap.
    fn drop_event(&mut self, seq: u64, at: ClockSample, reason: Reason) {
        self.dropped += 1;
        if let Some(g) = self.gap.as_mut() {
            g.to = seq;
            g.at = at;
        } else {
            tracing::warn!(
                reason = reason.as_str(),
                seq,
                "the outbox is full; events are dropped and a journal.gap will record the loss"
            );
            self.gap = Some(PendingGap {
                from: seq,
                to: seq,
                reason,
                at,
            });
        }
    }

    /// Whether `count` records of `bytes` in total can be stored now.
    fn admits(&self, bytes: usize, count: usize, class: Class) -> Result<(), Reason> {
        if let Some(sp) = self.spool.as_ref().filter(|s| s.writable()) {
            let size = bytes as u64 + RECORD_OVERHEAD * count as u64;
            let limit = match class {
                Class::Normal => self
                    .budget
                    .spool_bytes
                    .saturating_sub(self.budget.spool_reserve),
                Class::Final => self.budget.spool_bytes.saturating_sub(GAP_RECORD_MAX),
                Class::Gap => self.budget.spool_bytes,
            };
            return if sp.written() + size <= limit {
                Ok(())
            } else {
                Err(Reason::SpoolFull)
            };
        }
        if !self.caught_up() {
            return Err(if self.spool.is_some() {
                Reason::SpoolError
            } else {
                Reason::WindowFull
            });
        }
        if self.window_fits(count, bytes, class) {
            Ok(())
        } else {
            Err(Reason::WindowFull)
        }
    }

    /// Stores one event (its `seq` is above everything stored): on disk when there is a spool (and
    /// in the window too if the window is caught up and has room), else in the window.
    fn store(&mut self, e: EventEnvelope, bytes: &[u8], class: Class) -> bool {
        let cache = self.caught_up() && self.window_fits(1, bytes.len(), Class::Normal);
        if let Some(sp) = self.spool.as_mut().filter(|s| s.writable()) {
            match sp.append(e.seq, bytes) {
                Ok(()) => {
                    self.last_seq = e.seq;
                    if cache {
                        self.window_push(e, bytes.len());
                    }
                    return true;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "the spool is no longer writable; new events can only wait in memory");
                    sp.close_writer();
                }
            }
        }
        // No usable spool: only the window can hold it, and only at its tail.
        if self.caught_up() && self.window_fits(1, bytes.len(), class) {
            self.last_seq = e.seq;
            self.window_push(e, bytes.len());
            true
        } else {
            false
        }
    }

    /// The window covers everything stored, so an event can join its tail without leaving a hole.
    const fn caught_up(&self) -> bool {
        self.covered_through == self.last_seq
    }

    fn window_fits(&self, count: usize, bytes: usize, class: Class) -> bool {
        let (events, size) = match class {
            Class::Normal => (self.budget.window_events, self.budget.window_bytes),
            Class::Final | Class::Gap => (
                self.budget.window_events + self.budget.reserve_events,
                self.budget.window_bytes + self.budget.reserve_bytes,
            ),
        };
        self.window.len() + count <= events && self.window_bytes + bytes <= size
    }

    /// Appends the newest event (or one read back in order) and extends the coverage to it.
    fn window_push(&mut self, event: EventEnvelope, bytes: usize) {
        self.covered_through = event.seq;
        self.window_bytes += bytes;
        self.window.push_back(Slot { event, bytes });
    }

    fn window_pop(&mut self) {
        if let Some(s) = self.window.pop_front() {
            self.window_bytes -= s.bytes;
            self.covers_after = self.covers_after.max(s.event.seq);
        }
    }

    /// The server committed everything up to `through`.
    pub(crate) fn ack(&mut self, through: u64) {
        self.acked_through = self.acked_through.max(through);
        while self
            .window
            .front()
            .is_some_and(|s| s.event.seq <= self.acked_through)
        {
            self.window_pop();
        }
        // Acked events need no coverage: the window now starts after them (and, when they were
        // never in it, is empty from there).
        self.covers_after = self.covers_after.max(self.acked_through);
        self.covered_through = self
            .covered_through
            .max(self.acked_through.min(self.last_seq));
        self.rotate();
        if self.sealed {
            self.emit_gap();
        }
    }

    /// Starts the spool over when everything in it has been acked.
    fn rotate(&mut self) {
        let Some(sp) = self.spool.as_mut() else {
            return;
        };
        if !sp.writable()
            || sp.through() > self.acked_through
            || sp.written() < self.budget.rotate_bytes
        {
            return;
        }
        if let Err(e) = sp.reset(self.last_seq + 1) {
            tracing::warn!(error = %e, "cannot restart the spool; continuing without it");
            self.spool = None;
        }
    }

    /// No more events will come: store the gap marker for dropped events, if any, as soon as
    /// possible.
    pub(crate) fn seal(&mut self) {
        self.sealed = true;
        self.emit_gap();
    }

    /// Stores the pending `journal.gap` marker, if it fits.
    fn emit_gap(&mut self) {
        let Some(g) = self.gap.as_ref() else {
            return;
        };
        let event = g.event();
        let Some(bytes) = encode(&event) else {
            return;
        };
        if self.admits(bytes.len(), 1, Class::Gap).is_ok() && self.store(event, &bytes, Class::Gap)
        {
            self.gap = None;
        }
    }

    /// Unacked events with `seq > after`, at most `max`. Reads the spool back into the window
    /// when the window has run dry; a spool that can't be read stalls delivery (and is logged)
    /// instead of skipping events.
    pub(crate) fn after(&mut self, after: u64, max: usize) -> Vec<EventEnvelope> {
        let after = after.max(self.acked_through);
        if max == 0 || after >= self.last_seq {
            return Vec::new();
        }
        if !self.covers(after) {
            self.refill(after);
            if !self.covers(after) {
                return Vec::new();
            }
        }
        let first = self.window.partition_point(|s| s.event.seq <= after);
        self.window
            .range(first..)
            .take(max)
            .map(|s| s.event.clone())
            .collect()
    }

    /// The window holds every stored event after `after`, up to `covered_through`, and at least
    /// one of them.
    const fn covers(&self, after: u64) -> bool {
        self.covers_after <= after && after < self.covered_through
    }

    /// Loads events from the spool so the window covers `after`.
    fn refill(&mut self, after: u64) {
        // Taken out so the window can be edited while the spool is read.
        let Some(mut sp) = self.spool.take() else {
            // Memory-only: the window already holds everything unacked.
            return;
        };
        self.refill_from(&mut sp, after);
        self.spool = Some(sp);
    }

    fn refill_from(&mut self, sp: &mut Spool, after: u64) {
        if after >= sp.through() {
            return;
        }
        if self.covered_through == after {
            // Reading on from the end of the window. Sent events at the front are on disk and
            // can be read again after a rewind; make room for one more event.
            while self.window.front().is_some_and(|s| s.event.seq <= after)
                && (self.window.len() >= self.budget.window_events
                    || self.window_bytes + self.budget.max_event_bytes > self.budget.window_bytes)
            {
                self.window_pop();
            }
        } else {
            if self
                .window
                .back()
                .is_some_and(|b| b.event.seq > sp.through())
            {
                // Events newer than the file live only here; they can't be dropped for a rewind.
                tracing::warn!(after, "cannot rewind: newer events exist only in memory");
                return;
            }
            self.window.clear();
            self.window_bytes = 0;
            self.covers_after = after;
            self.covered_through = after;
        }
        let room = self.budget.window_events.saturating_sub(self.window.len());
        let bytes = self.budget.window_bytes.saturating_sub(self.window_bytes);
        match sp.read(
            after,
            room.min(REFILL_EVENTS),
            bytes,
            self.window.is_empty(),
        ) {
            Ok(events) => {
                for (event, n) in events {
                    self.window_push(event, n);
                }
            }
            Err(e) => tracing::warn!(error = %e, after, "cannot read the spool back"),
        }
    }

    /// Highest acked `seq`.
    pub(crate) const fn acked_through(&self) -> u64 {
        self.acked_through
    }

    /// Highest `seq` stored.
    pub(crate) const fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The `seq` the next event will have: after everything stored and any dropped run.
    pub(crate) fn next_seq(&self) -> u64 {
        self.last_seq.max(self.gap.as_ref().map_or(0, |g| g.to)) + 1
    }

    /// Everything stored has been acked and no gap marker is waiting to be stored.
    pub(crate) const fn drained(&self) -> bool {
        self.acked_through >= self.last_seq && self.gap.is_none()
    }

    /// The `seq` the server needs next: the one after the last ack, while anything (an event or
    /// a pending marker) is unacked. It may be a slot a marker covers.
    pub(crate) const fn oldest_unacked(&self) -> Option<u64> {
        if self.acked_through < self.last_seq || self.gap.is_some() {
            Some(self.acked_through + 1)
        } else {
            None
        }
    }

    /// Events dropped so far.
    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }
}

fn encode(e: &EventEnvelope) -> Option<Vec<u8>> {
    serde_json::to_vec(e).ok()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write as _,
        path::PathBuf,
        sync::atomic::{AtomicU32, Ordering},
    };

    use fleet_protocol::event::RunSignal;
    use proptest::prelude::*;
    use time::OffsetDateTime;

    use super::*;

    /// A temp directory removed on drop, unique per call.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "fleet-outbox-{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        fn spool(&self) -> PathBuf {
            self.0.join("spool.bin")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// An event with `seq`.
    fn ev(seq: u64) -> EventEnvelope {
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

    fn padded(seq: u64, pad: usize) -> EventEnvelope {
        let mut e = ev(seq);
        e.data = serde_json::json!({ "pad": "x".repeat(pad) });
        e
    }

    fn run_ended(seq: u64) -> EventEnvelope {
        let mut e = ev(seq);
        e.kind = RUN_ENDED.into();
        e
    }

    /// Small enough for tests to hit every limit with a few dozen events (an event is ~200 B).
    fn tiny() -> Budget {
        Budget {
            spool_bytes: 4000,
            spool_reserve: 1500,
            rotate_bytes: u64::MAX,
            window_events: 4,
            window_bytes: 1200,
            reserve_events: 2,
            reserve_bytes: 1000,
            max_event_bytes: 600,
        }
    }

    fn seqs(events: &[EventEnvelope]) -> Vec<u64> {
        events.iter().map(|e| e.seq).collect()
    }

    /// The server's ingest rule: a duplicate is skipped, the next seq or a marker that covers
    /// exactly the hole before it is applied, anything else is a `seq_gap`.
    #[derive(Default)]
    struct Server {
        log: Vec<EventEnvelope>,
    }

    impl Server {
        fn committed(&self) -> u64 {
            self.log.last().map_or(0, |e| e.seq)
        }

        /// Applies a batch; `Err(expected)` stops at a hole, as `error{seq_gap}` does.
        fn apply(&mut self, batch: Vec<EventEnvelope>) -> Result<(), u64> {
            for e in batch {
                let c = self.committed();
                let covers = gap_of(&e).is_some_and(|g| {
                    g.to_seq == e.seq && g.from_seq == c + 1 && g.from_seq <= g.to_seq
                });
                if e.seq <= c {
                    continue;
                }
                if e.seq != c + 1 && !covers {
                    return Err(c + 1);
                }
                self.log.push(e);
            }
            Ok(())
        }
    }

    /// What a server would end up with after everything is delivered: sends from `sent`,
    /// applies the contiguous prefix, acks. Returns the committed log.
    fn deliver_all(o: &mut Outbox) -> Vec<EventEnvelope> {
        let mut server = Server::default();
        let mut sent = 0;
        for _ in 0..1_000_000 {
            let batch = o.after(sent, 64);
            if batch.is_empty() {
                break;
            }
            sent = batch.last().unwrap().seq;
            server
                .apply(batch)
                .expect("the stream has an unexplained hole");
            o.ack(server.committed());
        }
        assert!(o.drained(), "everything delivered and acked");
        server.log
    }

    /// The seqs a committed log accounts for: an event counts as itself, a marker as its range.
    /// For a correct outbox this is exactly `1..=n`, in order, each once.
    fn accounted(log: &[EventEnvelope]) -> Vec<u64> {
        let mut out = Vec::new();
        for e in log {
            match gap_of(e) {
                Some(g) => out.extend(g.from_seq..=g.to_seq),
                None => out.push(e.seq),
            }
        }
        out
    }

    fn gap_of(e: &EventEnvelope) -> Option<JournalGap> {
        (e.kind == "journal.gap").then(|| serde_json::from_value(e.data.clone()).unwrap())
    }

    fn gaps(log: &[EventEnvelope]) -> Vec<JournalGap> {
        log.iter().filter_map(gap_of).collect()
    }

    #[test]
    fn acks_release_events_in_order() {
        let mut o = Outbox::memory(Budget::default());
        o.push((1..=5).map(ev).collect());
        assert_eq!(seqs(&o.after(0, 2)), [1, 2]);
        o.ack(3);
        assert_eq!(o.oldest_unacked(), Some(4));
        o.ack(2);
        assert_eq!(o.acked_through(), 3, "acks never go backwards");
        assert_eq!(seqs(&o.after(0, 10)), [4, 5], "acked events are not resent");
        o.ack(5);
        assert!(o.drained());
        assert_eq!(o.oldest_unacked(), None);
    }

    #[test]
    fn spool_round_trips_and_survives_a_torn_tail() {
        let dir = Dir::new("torn");
        let id = Uuid::from_u128(42);
        let mut o = Outbox::with_spool(&dir.spool(), id, Budget::default());
        o.push((1..=3).map(ev).collect());
        let mut bytes = fs::read(dir.spool()).unwrap();
        bytes.truncate(bytes.len() - 5); // a crash mid-record
        fs::write(dir.spool(), bytes).unwrap();
        let (read_id, mut replay) = Outbox::replay(&dir.spool()).unwrap();
        assert_eq!(read_id, id);
        assert_eq!(replay.last_seq(), 2);
        assert_eq!(seqs(&replay.after(0, 10)), [1, 2]);
    }

    #[test]
    fn a_spool_from_before_first_seq_existed_still_replays() {
        let dir = Dir::new("legacy");
        let id = Uuid::from_u128(7);
        let mut f = fs::File::create(dir.spool()).unwrap();
        let mut record = |body: &[u8]| {
            let len = u32::try_from(body.len()).unwrap();
            f.write_all(&len.to_be_bytes()).unwrap();
            f.write_all(&crc32fast::hash(body).to_be_bytes()).unwrap();
            f.write_all(body).unwrap();
        };
        record(format!(r#"{{"agent_run_id":"{id}"}}"#).as_bytes());
        for seq in 1..=3 {
            record(&serde_json::to_vec(&ev(seq)).unwrap());
        }
        drop(f);
        let (read_id, mut o) = Outbox::replay(&dir.spool()).unwrap();
        assert_eq!(read_id, id);
        assert_eq!(seqs(&o.after(0, 10)), [1, 2, 3]);
        assert_eq!(o.oldest_unacked(), Some(1));
    }

    #[test]
    fn a_full_spool_drops_bodies_and_says_so_with_one_sequenced_gap() {
        let dir = Dir::new("full");
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), tiny());
        o.push((1..=40).map(ev).collect());
        assert!(o.dropped() > 0, "the spool filled up");
        assert!(o.window.len() <= tiny().window_events + tiny().reserve_events);
        assert!(fs::metadata(dir.spool()).unwrap().len() <= tiny().spool_bytes);
        let stored = o.last_seq();
        o.push(vec![run_ended(41)]);
        o.seal();
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=41).collect::<Vec<_>>());
        let g = gaps(&log);
        assert_eq!(g.len(), 1, "one contiguous run of dropped events, one gap");
        assert_eq!((g[0].from_seq, g[0].to_seq), (stored + 1, 40));
        assert_eq!(g[0].reason, "spool_full");
        let last = log.last().unwrap();
        assert_eq!(last.kind, RUN_ENDED, "run.ended is stored from the reserve");
        assert_eq!(log[log.len() - 2].kind, "journal.gap");
        // One seq space: the events keep their numbers, and the marker sits in the last
        // dropped slot, so the delivered seqs are the stored ones, then 40 (marker), then 41.
        let mut expected: Vec<u64> = (1..=stored).collect();
        expected.extend([40, 41]);
        assert_eq!(seqs(&log), expected);
        assert_eq!(log[log.len() - 2].seq, g[0].to_seq);
    }

    #[test]
    fn dropped_events_leave_holes_that_survive_refills_rewinds_and_replay() {
        let dir = Dir::new("holes");
        let budget = Budget {
            spool_bytes: 1 << 22,
            window_events: 8,
            window_bytes: 1 << 20,
            ..tiny()
        };
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), budget);
        // Every 7th event is too large to store, so holes (and their markers) are spread over
        // the file and across index slots.
        let total = 1700;
        for seq in 1..=total {
            o.push(vec![padded(seq, if seq % 7 == 0 { 5000 } else { 0 })]);
        }
        o.seal();
        assert_eq!(o.dropped(), total / 7);
        assert_eq!(o.next_seq(), total + 1);
        // Reading in small steps, with rewinds, never skips a stored event or a marker.
        let mut server = Server::default();
        let mut sent = 0;
        let mut steps = 0;
        while sent < o.last_seq() {
            let batch = o.after(sent, 3);
            assert_ne!(batch.len(), 0);
            sent = batch.last().unwrap().seq;
            server.apply(batch).unwrap();
            steps += 1;
            if steps % 50 == 0 {
                // A reconnect rewind to a committed point far behind the window: what the outbox
                // sends from there must satisfy a server that stopped at that point.
                let back = server.log.len() / 3;
                let mut behind = Server {
                    log: server.log[..=back].to_vec(),
                };
                let batch = o.after(behind.committed(), 5);
                assert!(batch[0].seq > behind.committed());
                behind.apply(batch).unwrap();
                sent = server.committed();
            }
        }
        assert_eq!(accounted(&server.log), (1..=total).collect::<Vec<_>>());
        // The same spool replays from disk with its holes.
        let (_, mut replay) = Outbox::replay(&dir.spool()).unwrap();
        assert_eq!(replay.last_seq(), o.last_seq());
        let log = deliver_all(&mut replay);
        assert_eq!(accounted(&log), (1..=total).collect::<Vec<_>>());
        assert_eq!(gaps(&log).len(), usize::try_from(total / 7).unwrap());
    }

    #[test]
    fn a_spool_with_an_unexplained_hole_replays_only_up_to_it() {
        let dir = Dir::new("badhole");
        let mut f = fs::File::create(dir.spool()).unwrap();
        let mut record = |body: &[u8]| {
            let len = u32::try_from(body.len()).unwrap();
            f.write_all(&len.to_be_bytes()).unwrap();
            f.write_all(&crc32fast::hash(body).to_be_bytes()).unwrap();
            f.write_all(body).unwrap();
        };
        record(format!(r#"{{"agent_run_id":"{}"}}"#, Uuid::nil()).as_bytes());
        for seq in [1, 2, 4, 5] {
            record(&serde_json::to_vec(&ev(seq)).unwrap());
        }
        drop(f);
        let (_, mut o) = Outbox::replay(&dir.spool()).unwrap();
        assert_eq!(
            seqs(&o.after(0, 10)),
            [1, 2],
            "seq 4 follows nothing that covers 3"
        );
    }

    #[test]
    fn the_resume_state_names_a_pending_gap() {
        let mut o = Outbox::memory(tiny());
        o.push((1..=10).map(ev).collect());
        assert_eq!(o.dropped(), 6, "the window holds four events");
        o.ack(4);
        // Events 5..=10 were dropped and the marker isn't stored yet: the next event is 11, and
        // the server still needs 5 (the hole's start), so it doesn't call it a discarded range.
        assert_eq!(
            (o.next_seq(), o.oldest_unacked(), o.drained()),
            (11, Some(5), false)
        );
        o.seal();
        assert_eq!((o.last_seq(), o.oldest_unacked()), (10, Some(5)));
        o.ack(10);
        assert!(o.drained());
        assert_eq!(o.oldest_unacked(), None);
    }

    #[test]
    fn a_gap_is_emitted_at_seal_when_no_event_follows_it() {
        let dir = Dir::new("seal");
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), tiny());
        o.push((1..=40).map(ev).collect());
        o.seal();
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=40).collect::<Vec<_>>());
        assert_eq!(gaps(&log).len(), 1);
    }

    #[test]
    fn without_a_spool_the_window_is_the_bound() {
        let mut o = Outbox::memory(tiny());
        for chunk in (1..=100u64).collect::<Vec<_>>().chunks(10) {
            o.push(chunk.iter().copied().map(ev).collect());
            assert!(o.window.len() <= 4 + 2 && o.window_bytes <= 1200 + 1000);
        }
        assert_eq!(o.dropped(), 96, "only the window's four events fit");
        o.seal();
        // Nothing has been acked, so the gap waits for room in the window; the acks make it.
        assert!(!o.drained());
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=100).collect::<Vec<_>>());
        assert_eq!(gaps(&log).len(), 1);
        assert_eq!(gaps(&log)[0].reason, "window_full");
    }

    #[test]
    fn a_spool_that_cannot_be_created_degrades_to_memory() {
        let dir = Dir::new("nospool");
        let mut o = Outbox::with_spool(&dir.0.join("missing/spool.bin"), Uuid::nil(), tiny());
        o.push((1..=20).map(ev).collect());
        assert!(o.dropped() > 0);
        o.seal();
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn oversize_and_unusable_events_become_gaps_not_silence() {
        let mut o = Outbox::memory(tiny());
        o.push(vec![ev(1), padded(2, 5000), ev(3)]);
        o.seal();
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), [1, 2, 3]);
        let g = gaps(&log);
        assert_eq!((g[0].from_seq, g[0].to_seq), (2, 2));
        assert_eq!(g[0].reason, "event_too_large");
        assert_eq!(
            seqs(&log),
            [1, 2, 3],
            "the marker sits in the dropped event's slot"
        );
    }

    #[test]
    fn a_gap_record_always_fits_its_reserve() {
        let worst = PendingGap {
            from: u64::MAX,
            to: u64::MAX,
            reason: Reason::Unserializable,
            at: ClockSample {
                wall: OffsetDateTime::UNIX_EPOCH,
                mono_ms: u64::MAX,
            },
        }
        .event();
        let n = encode(&worst).unwrap().len() as u64 + RECORD_OVERHEAD;
        assert!(n <= GAP_RECORD_MAX, "{n}");
        let b = Budget::default();
        assert!(b.spool_reserve > GAP_RECORD_MAX + b.max_event_bytes as u64);
        assert!(b.reserve_bytes > b.max_event_bytes);
    }

    #[test]
    fn the_window_refills_from_the_spool_and_a_rewind_rereads_it() {
        let dir = Dir::new("refill");
        let budget = Budget {
            spool_bytes: 1 << 20,
            window_events: 8,
            window_bytes: 1 << 20,
            ..tiny()
        };
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), budget);
        o.push((1..=1000).map(ev).collect());
        assert_eq!(o.dropped(), 0);
        assert!(o.window.len() <= 8, "events beyond the window are on disk");
        // Walk the whole backlog, window by window.
        let mut sent = 0;
        while sent < 1000 {
            let batch = o.after(sent, 3);
            assert_eq!(batch[0].seq, sent + 1);
            sent = batch.last().unwrap().seq;
            assert!(o.window.len() <= 8);
        }
        // The server lost its tail (a SeqGap): rewind far behind the window, across index slots.
        for from in [0, 499, 300, 999] {
            assert_eq!(o.after(from, 2)[0].seq, from + 1);
        }
        o.ack(1000);
        assert!(o.drained());
    }

    #[test]
    fn the_spool_starts_over_once_everything_is_acked() {
        let dir = Dir::new("rotate");
        let budget = Budget {
            rotate_bytes: 2000,
            window_events: 4,
            ..tiny()
        };
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), budget);
        let mut next = 1;
        for _ in 0..30 {
            o.push((next..next + 5).map(ev).collect());
            next += 5;
            while !o.drained() {
                let batch = o.after(o.acked_through(), 100);
                o.ack(batch.last().unwrap().seq);
            }
            assert!(fs::metadata(dir.spool()).unwrap().len() <= budget.spool_bytes);
        }
        assert_eq!(o.dropped(), 0, "a healthy connection never fills the spool");
        assert_eq!(o.last_seq(), 150);
        // The file now begins after the acked events, and replays from there.
        o.push(vec![ev(151), ev(152)]);
        let (_, mut replay) = Outbox::replay(&dir.spool()).unwrap();
        let first = replay.oldest_unacked().unwrap();
        assert!(first > 1, "{first}");
        assert_eq!(seqs(&replay.after(0, 100)).last(), Some(&152));
    }

    #[test]
    fn a_dead_spool_keeps_the_prefix_readable() {
        let dir = Dir::new("dead");
        let budget = Budget {
            window_events: 3,
            ..tiny()
        };
        let mut o = Outbox::with_spool(&dir.spool(), Uuid::nil(), budget);
        o.push((1..=10).map(ev).collect());
        o.spool.as_mut().unwrap().close_writer(); // as after a failed write
        o.push((11..=12).map(ev).collect());
        // The backlog is on disk, so the window isn't caught up: new events can't be placed.
        assert_eq!(o.dropped(), 2);
        o.seal();
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=12).collect::<Vec<_>>());
        assert_eq!(gaps(&log)[0].reason, "spool_error");
    }

    /// One step of a randomized run against a model server.
    #[derive(Debug, Clone)]
    enum Op {
        Push { n: usize, pad: usize },
        Send { max: usize },
        AckAll,
        AckHalf,
        Reconnect,
        Rewind { back: u64 },
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            4 => (1usize..12, 0usize..450).prop_map(|(n, pad)| Op::Push { n, pad }),
            4 => (1usize..10).prop_map(|max| Op::Send { max }),
            2 => Just(Op::AckAll),
            1 => Just(Op::AckHalf),
            1 => Just(Op::Reconnect),
            1 => (1u64..30).prop_map(|back| Op::Rewind { back }),
        ];
        proptest::collection::vec(op, 1..80)
    }

    fn check_bounds(o: &Outbox, b: &Budget, spool: Option<&PathBuf>) {
        assert!(o.window.len() <= b.window_events + b.reserve_events);
        assert!(o.window_bytes <= b.window_bytes + b.reserve_bytes);
        assert_eq!(
            o.window_bytes,
            o.window.iter().map(|s| s.bytes).sum::<usize>(),
            "byte accounting"
        );
        if let Some(p) = spool {
            assert!(fs::metadata(p).unwrap().len() <= b.spool_bytes);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Whatever the pushes, acks, sends and reconnects: memory and disk stay within the
        /// budget, the server's ingest rule is always satisfied, and every produced `seq` is
        /// either delivered or inside exactly one delivered `journal.gap`.
        #[test]
        fn a_bounded_outbox_loses_nothing_it_does_not_report(
            ops in ops(),
            with_spool in any::<bool>(),
            window_events in 1usize..8,
            end_with_run_ended in any::<bool>(),
        ) {
            let dir = Dir::new("prop");
            let budget = Budget { window_events, ..tiny() };
            let mut o = if with_spool {
                Outbox::with_spool(&dir.spool(), Uuid::nil(), budget)
            } else {
                Outbox::memory(budget)
            };
            let spool = with_spool.then(|| dir.spool());
            let mut produced = 0u64;
            let mut sent = 0u64;
            let mut server = Server::default();
            for op in ops {
                match op {
                    Op::Push { n, pad } => {
                        let batch = (0..n).map(|i| {
                            produced += 1;
                            padded(produced, if i % 3 == 0 { pad } else { 0 })
                        }).collect();
                        o.push(batch);
                    }
                    Op::Send { max } => {
                        let batch = o.after(sent, max);
                        if let Some(last) = batch.last() {
                            sent = last.seq;
                        }
                        if let Err(expected) = server.apply(batch) {
                            sent = expected - 1; // the server's seq_gap
                        }
                    }
                    Op::AckAll => o.ack(server.committed()),
                    Op::AckHalf => {
                        // Acks only ever name a committed seq, never a slot inside a hole.
                        let a = o.acked_through();
                        let later: Vec<u64> =
                            server.log.iter().map(|e| e.seq).filter(|&q| q > a).collect();
                        if let Some(q) = later.get(later.len() / 2) {
                            o.ack(*q);
                        }
                    }
                    Op::Reconnect => {
                        o.ack(server.committed()); // welcome.committed_through
                        sent = server.committed();
                    }
                    Op::Rewind { back } => sent = sent.saturating_sub(back).max(o.acked_through()),
                }
                check_bounds(&o, &budget, spool.as_ref());
            }
            if end_with_run_ended {
                produced += 1;
                o.push(vec![run_ended(produced)]);
            }
            o.seal();
            check_bounds(&o, &budget, spool.as_ref());
            // Finish: reconnect and deliver the rest.
            o.ack(server.committed());
            let mut sent = server.committed();
            for _ in 0..10_000 {
                let batch = o.after(sent, 7);
                if batch.is_empty() {
                    break;
                }
                sent = batch.last().unwrap().seq;
                prop_assert_eq!(server.apply(batch), Ok(()), "a hole the server can't explain");
                o.ack(server.committed());
                check_bounds(&o, &budget, spool.as_ref());
            }
            let log = server.log;
            prop_assert!(o.drained());
            prop_assert_eq!(accounted(&log), (1..=produced).collect::<Vec<_>>());
            let seqs: Vec<u64> = log.iter().map(|e| e.seq).collect();
            prop_assert!(seqs.windows(2).all(|w| w[0] < w[1]), "delivered seqs strictly increase");
            let reported: u64 = gaps(&log).iter().map(|g| g.to_seq - g.from_seq + 1).sum();
            prop_assert_eq!(reported, o.dropped(), "every dropped event is in exactly one gap");
        }
    }

    /// Resident set size of this process in bytes.
    fn rss() -> u64 {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let pid = Pid::from_u32(std::process::id());
        let mut sys = System::new().expect("sysinfo should initialise in tests");
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        sys.process(pid).map_or(0, sysinfo::Process::memory)
    }

    /// Offline: nothing ever acks. 200k events must leave memory flat and the gap accounting exact.
    fn offline_stress(mut o: Outbox, budget: &Budget, spool: Option<&PathBuf>) {
        const TOTAL: u64 = 200_000;
        let mut rss_mid = 0;
        for chunk in 0..TOTAL / 100 {
            let from = chunk * 100 + 1;
            o.push((from..from + 100).map(ev).collect());
            check_bounds_cheap(&o, budget);
            if from > 60_000 && rss_mid == 0 {
                rss_mid = rss();
            }
        }
        let growth = rss().saturating_sub(rss_mid);
        assert!(
            growth < 8 * 1024 * 1024,
            "RSS grew by {growth} bytes over 140k more events"
        );
        o.push(vec![run_ended(TOTAL + 1)]);
        o.seal();
        if let Some(p) = spool {
            assert!(fs::metadata(p).unwrap().len() <= budget.spool_bytes);
        }
        // The server finally connects and takes everything.
        let log = deliver_all(&mut o);
        assert_eq!(accounted(&log), (1..=TOTAL + 1).collect::<Vec<_>>());
        let g = gaps(&log);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].to_seq, TOTAL);
        assert_eq!(
            g[0].to_seq - g[0].from_seq + 1,
            o.dropped(),
            "the gap counts exactly the events dropped"
        );
        assert_eq!(log.last().map(|e| e.kind.as_str()), Some(RUN_ENDED));
    }

    fn check_bounds_cheap(o: &Outbox, b: &Budget) {
        assert!(o.window.len() <= b.window_events + b.reserve_events);
        assert!(o.window_bytes <= b.window_bytes + b.reserve_bytes);
    }

    #[test]
    fn stress_offline_with_a_spool_keeps_memory_flat() {
        let dir = Dir::new("stress-spool");
        let budget = Budget {
            spool_bytes: 8 * 1024 * 1024,
            ..Budget::default()
        };
        let o = Outbox::with_spool(&dir.spool(), Uuid::nil(), budget);
        offline_stress(o, &budget, Some(&dir.spool()));
    }

    #[test]
    fn stress_offline_without_a_spool_keeps_memory_flat() {
        let budget = Budget::default();
        offline_stress(Outbox::memory(budget), &budget, None);
    }
}
