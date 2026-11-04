// SPDX-License-Identifier: AGPL-3.0-only
//! The outbox: journal events waiting for the server's ack, plus an on-disk spool
//! (research 02 §7, research 05 §3.2; invariant I5).
//!
//! Events stay in memory until acked; nothing is dropped, so the journal sent is gap-free. Every
//! event is also appended to a spool file (`[len u32 BE][crc32 u32 BE][json]` records, the first
//! record naming the agent run), so `fleet-agent flush` can replay a run whose agent died. The
//! spool is capped; past the cap the agent stops writing it and says so.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufWriter, Read, Write},
    path::Path,
};

use fleet_protocol::journal::EventEnvelope;
use uuid::Uuid;

/// Default spool cap (PLAN §6.6).
pub(crate) const SPOOL_LIMIT: u64 = 64 * 1024 * 1024;

/// Events waiting for an ack.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    pending: VecDeque<EventEnvelope>,
    acked_through: u64,
    last_seq: u64,
    spool: Option<Spool>,
    /// The spool hit its cap.
    pub(crate) spool_full: bool,
}

impl Outbox {
    /// An outbox that also spools to `path`.
    pub(crate) fn with_spool(path: &Path, agent_run_id: Uuid, limit: u64) -> Self {
        let spool = Spool::create(path, agent_run_id, limit).ok();
        Self {
            spool,
            ..Self::default()
        }
    }

    /// Adds events (in `seq` order).
    pub(crate) fn push(&mut self, events: Vec<EventEnvelope>) {
        for e in events {
            if let Some(s) = self.spool.as_mut()
                && !s.append(&e)
            {
                self.spool_full = true;
                self.spool = None;
            }
            self.last_seq = self.last_seq.max(e.seq);
            self.pending.push_back(e);
        }
    }

    /// The server committed everything up to `through`.
    pub(crate) fn ack(&mut self, through: u64) {
        self.acked_through = self.acked_through.max(through);
        while self
            .pending
            .front()
            .is_some_and(|e| e.seq <= self.acked_through)
        {
            self.pending.pop_front();
        }
    }

    /// Unacked events with `seq > after`, at most `max`.
    pub(crate) fn after(&self, after: u64, max: usize) -> Vec<EventEnvelope> {
        self.pending
            .iter()
            .filter(|e| e.seq > after)
            .take(max)
            .cloned()
            .collect()
    }

    /// Highest acked `seq`.
    pub(crate) fn acked_through(&self) -> u64 {
        self.acked_through
    }

    /// Highest `seq` pushed.
    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Everything pushed has been acked.
    pub(crate) fn drained(&self) -> bool {
        self.pending.is_empty()
    }

    /// Oldest unacked `seq`.
    pub(crate) fn oldest_unacked(&self) -> Option<u64> {
        self.pending.front().map(|e| e.seq)
    }
}

#[derive(Debug)]
struct Spool {
    file: BufWriter<File>,
    written: u64,
    limit: u64,
}

impl Spool {
    fn create(path: &Path, agent_run_id: Uuid, limit: u64) -> io::Result<Self> {
        let mut s = Self {
            file: BufWriter::new(File::create(path)?),
            written: 0,
            limit,
        };
        let header = serde_json::to_vec(&serde_json::json!({ "agent_run_id": agent_run_id }))
            .map_err(io::Error::other)?;
        s.write_record(&header)?;
        Ok(s)
    }

    /// Appends one event; false once the cap is reached or a write fails.
    fn append(&mut self, e: &EventEnvelope) -> bool {
        let Ok(bytes) = serde_json::to_vec(e) else {
            return true;
        };
        if self.written + bytes.len() as u64 + 8 > self.limit {
            return false;
        }
        self.write_record(&bytes).is_ok()
    }

    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = u32::try_from(bytes.len()).map_err(io::Error::other)?;
        self.file.write_all(&len.to_be_bytes())?;
        self.file.write_all(&crc32fast::hash(bytes).to_be_bytes())?;
        self.file.write_all(bytes)?;
        // Flushed per record: the spool exists for the case where this process dies.
        self.file.flush()?;
        self.written += bytes.len() as u64 + 8;
        Ok(())
    }
}

/// Reads a spool: the run ID and every intact event. A torn or corrupt tail is ignored.
pub(crate) fn read_spool(mut r: impl Read) -> io::Result<(Uuid, Vec<EventEnvelope>)> {
    let mut records = Vec::new();
    loop {
        let mut head = [0u8; 8];
        if r.read_exact(&mut head).is_err() {
            break;
        }
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let crc = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() || crc32fast::hash(&body) != crc {
            break;
        }
        records.push(body);
    }
    let mut it = records.into_iter();
    let header: serde_json::Value =
        serde_json::from_slice(&it.next().ok_or_else(|| io::Error::other("empty spool"))?)
            .map_err(io::Error::other)?;
    let id = header["agent_run_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("spool has no agent_run_id"))?;
    let events = it.filter_map(|b| serde_json::from_slice(&b).ok()).collect();
    Ok((id, events))
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{
        event::{EventBody, RunSignal},
        journal::{ClockSample, Provenance, SourceKind, Subject},
    };
    use time::OffsetDateTime;

    use super::*;

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

    #[test]
    fn acks_release_events_in_order() {
        let mut o = Outbox::default();
        o.push((1..=5).map(ev).collect());
        assert_eq!(
            o.after(0, 2).iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        o.ack(3);
        assert_eq!(o.oldest_unacked(), Some(4));
        o.ack(2);
        assert_eq!(o.acked_through(), 3, "acks never go backwards");
        o.ack(5);
        assert!(o.drained());
    }

    #[test]
    fn spool_round_trips_and_survives_a_torn_tail() {
        let path = std::env::temp_dir().join(format!("fleet-spool-{}", std::process::id()));
        let id = Uuid::from_u128(42);
        let mut o = Outbox::with_spool(&path, id, SPOOL_LIMIT);
        o.push((1..=3).map(ev).collect());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 5); // a crash mid-record
        let (read_id, events) = read_spool(bytes.as_slice()).unwrap();
        assert_eq!(read_id, id);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_full_spool_stops_but_keeps_events_in_memory() {
        let path = std::env::temp_dir().join(format!("fleet-spool-full-{}", std::process::id()));
        let mut o = Outbox::with_spool(&path, Uuid::nil(), 600);
        o.push((1..=10).map(ev).collect());
        assert!(o.spool_full);
        assert_eq!(o.after(0, 100).len(), 10, "memory keeps everything");
        std::fs::remove_file(&path).ok();
    }
}
