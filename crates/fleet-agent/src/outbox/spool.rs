// SPDX-License-Identifier: AGPL-3.0-only
//! The on-disk half of the outbox: an append-only spool of `[len u32 BE][crc32 u32 BE][json]`
//! records. The first record is a header naming the agent run (`{"agent_run_id", "first_seq"}`;
//! spools written before `first_seq` existed start at 1; everything before `first_seq` was acked
//! and is not in the file). Every following record is one event, stored under its own `seq`.
//! Seqs strictly increase but may skip: a hole is the range of a `journal.gap` record, which is
//! stored at the hole's last `seq`, so a seq cannot be turned into an offset by arithmetic.
//!
//! The spool is the source of truth for events that are not in the outbox's send window: the
//! window is refilled from here ([`Spool::read`]) and `fleet-agent flush` replays a dead run from
//! it ([`Spool::open_replay`]). Reads use their own file handle and never disturb the writer.

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use fleet_protocol::{event::JournalGap, journal::EventEnvelope};
use serde::Deserialize;
use uuid::Uuid;

/// One index entry per this many records.
///
/// The index is what lets a reconnect rewind to any `seq` without scanning the file: each entry is
/// the `(seq, offset)` of a record, and a read starts at the last entry at or before the `seq` it
/// wants and skips at most `INDEX_STRIDE - 1` records. At 16 bytes per 256 records it is
/// `16 B × records / 256`: for the 64 MiB default budget and a record of at least ~150 bytes (an
/// envelope without a payload) that is under 28 KiB, and it is cleared whenever the spool is
/// rotated. Sequential reads (the common case) use a cursor instead.
pub(super) const INDEX_STRIDE: u64 = 256;

/// Bytes of framing per record: length and CRC.
pub(super) const RECORD_OVERHEAD: u64 = 8;

/// A spool, writable while it can be and always readable for what it already holds.
#[derive(Debug)]
pub(super) struct Spool {
    path: PathBuf,
    /// `None` once a write failed (or for a replayed spool): the spool only serves reads.
    writer: Option<BufWriter<File>>,
    /// Opened on the first read.
    reader: Option<File>,
    run_id: Uuid,
    /// Everything before this `seq` was acked and is not in the file.
    first_seq: u64,
    /// Highest `seq` durably in the file (`first_seq - 1` when empty).
    through: u64,
    /// File length in bytes, header included.
    written: u64,
    /// Records in the file after the header.
    records: u64,
    /// `(seq, file offset)` of every `INDEX_STRIDE`-th record, starting with the first.
    index: Vec<(u64, u64)>,
    /// Where the previous read stopped: the last `seq` it consumed and the next record's offset.
    cursor: Option<(u64, u64)>,
}

#[derive(Deserialize)]
struct Header {
    agent_run_id: Uuid,
    #[serde(default = "one")]
    first_seq: u64,
}

/// Enough of a record to check it continues the journal.
#[derive(Deserialize)]
struct Peek {
    seq: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

impl Peek {
    /// Whether this record can follow one at `through`: the next seq, or a `journal.gap` that
    /// covers the hole (sequenced at its own `to_seq`, starting right after `through`).
    fn follows(&self, through: u64) -> bool {
        if self.seq == through + 1 {
            return true;
        }
        self.seq > through
            && self.kind == "journal.gap"
            && serde_json::from_value::<JournalGap>(self.data.clone())
                .is_ok_and(|g| g.to_seq == self.seq && g.from_seq == through + 1)
    }
}

const fn one() -> u64 {
    1
}

impl Spool {
    /// Creates (or truncates) the spool at `path` for `agent_run_id`.
    pub(super) fn create(path: &Path, run_id: Uuid) -> io::Result<Self> {
        let mut s = Self {
            path: path.to_owned(),
            writer: Some(BufWriter::new(File::create(path)?)),
            reader: None,
            run_id,
            first_seq: 1,
            through: 0,
            written: 0,
            records: 0,
            index: Vec::new(),
            cursor: None,
        };
        s.write_header()?;
        Ok(s)
    }

    /// Opens an existing spool read-only, indexing every intact event. A torn or corrupt tail
    /// (a crash mid-record, a bad CRC, a record that doesn't continue the journal) ends the
    /// spool there.
    pub(super) fn open_replay(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut r = BufReader::new(file);
        let mut buf = Vec::new();
        let n = read_record(&mut r, &mut buf, len)
            .ok_or_else(|| io::Error::other("empty spool or unreadable header"))?;
        let header: Header = serde_json::from_slice(&buf).map_err(io::Error::other)?;
        let first_seq = header.first_seq.max(1);
        let mut s = Self {
            path: path.to_owned(),
            writer: None,
            reader: None,
            run_id: header.agent_run_id,
            first_seq,
            through: first_seq - 1,
            written: RECORD_OVERHEAD + n as u64,
            records: 0,
            index: Vec::new(),
            cursor: None,
        };
        while let Some(n) = read_record(&mut r, &mut buf, len) {
            let Ok(peek) = serde_json::from_slice::<Peek>(&buf) else {
                break;
            };
            if !peek.follows(s.through) {
                break;
            }
            s.note(peek.seq, s.written);
            s.written += RECORD_OVERHEAD + n as u64;
        }
        Ok(s)
    }

    /// The agent run named in the header.
    pub(super) const fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// Everything before this `seq` was acked and is not in the file.
    pub(super) const fn first_seq(&self) -> u64 {
        self.first_seq
    }

    /// Highest `seq` in the file.
    pub(super) const fn through(&self) -> u64 {
        self.through
    }

    /// File length in bytes.
    pub(super) const fn written(&self) -> u64 {
        self.written
    }

    /// Whether appends still work.
    pub(super) const fn writable(&self) -> bool {
        self.writer.is_some()
    }

    /// Stops appending (after a failed write); the file stays readable.
    pub(super) fn close_writer(&mut self) {
        self.writer = None;
    }

    /// Counts the record for `seq` at `offset`.
    fn note(&mut self, seq: u64, offset: u64) {
        if self.records.is_multiple_of(INDEX_STRIDE) {
            self.index.push((seq, offset));
        }
        self.records += 1;
        self.through = seq;
    }

    /// Appends the record for `seq`, which must be above everything in the file, and flushes
    /// it: the spool exists for the case where this process dies.
    pub(super) fn append(&mut self, seq: u64, json: &[u8]) -> io::Result<()> {
        if seq <= self.through {
            return Err(io::Error::other("spool records must have rising seqs"));
        }
        let offset = self.written;
        self.write_record(json)?;
        self.note(seq, offset);
        Ok(())
    }

    /// Starts the file over with `first_seq` as its first event. Only valid once nothing in it is
    /// needed any more (everything acked).
    pub(super) fn reset(&mut self, first_seq: u64) -> io::Result<()> {
        self.writer = None;
        self.writer = Some(BufWriter::new(File::create(&self.path)?));
        self.reader = None;
        self.first_seq = first_seq;
        self.through = first_seq - 1;
        self.written = 0;
        self.records = 0;
        self.index.clear();
        self.cursor = None;
        self.write_header()
    }

    /// Reads events with `seq > after`, in order: at most `max_events`, stopping before the event
    /// that would take the total past `max_bytes`. With `at_least_one`, the first event is
    /// returned even if it alone is over `max_bytes` (so a reader that is empty always makes
    /// progress). Each event comes with its record size.
    pub(super) fn read(
        &mut self,
        after: u64,
        max_events: usize,
        max_bytes: usize,
        at_least_one: bool,
    ) -> io::Result<Vec<(EventEnvelope, usize)>> {
        let mut out = Vec::new();
        if after >= self.through {
            return Ok(out);
        }
        // Start at the last index entry that can still hold `after + 1`, or at the cursor if it
        // is further on.
        let slot = self.index.partition_point(|&(seq, _)| seq <= after + 1);
        let mut offset = self
            .index
            .get(slot.saturating_sub(1))
            .map(|&(_, o)| o)
            .ok_or_else(|| io::Error::other("spool index is missing"))?;
        let mut last = 0;
        if let Some((c_last, c_offset)) = self.cursor
            && c_last <= after
            && c_offset > offset
        {
            (last, offset) = (c_last, c_offset);
        }
        if self.reader.is_none() {
            self.reader = Some(File::open(&self.path)?);
        }
        let file = self
            .reader
            .as_ref()
            .ok_or_else(|| io::Error::other("spool reader"))?;
        let mut r = BufReader::new(file);
        r.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        let mut bytes = 0usize;
        while last < self.through && out.len() < max_events {
            let n = read_record(&mut r, &mut buf, self.written)
                .ok_or_else(|| io::Error::other("spool record is unreadable"))?;
            let seq = serde_json::from_slice::<Seq>(&buf)
                .map_err(io::Error::other)?
                .seq;
            if seq <= last {
                return Err(io::Error::other("spool record has a falling seq"));
            }
            if seq > after {
                if bytes + n > max_bytes && !(out.is_empty() && at_least_one) {
                    break;
                }
                let event: EventEnvelope =
                    serde_json::from_slice(&buf).map_err(io::Error::other)?;
                bytes += n;
                out.push((event, n));
            }
            last = seq;
            offset += RECORD_OVERHEAD + n as u64;
        }
        self.cursor = Some((last, offset));
        Ok(out)
    }

    fn write_header(&mut self) -> io::Result<()> {
        let header = serde_json::to_vec(&serde_json::json!({
            "agent_run_id": self.run_id,
            "first_seq": self.first_seq,
        }))
        .map_err(io::Error::other)?;
        self.write_record(&header)
    }

    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("spool is not writable"))?;
        let len = u32::try_from(bytes.len()).map_err(io::Error::other)?;
        w.write_all(&len.to_be_bytes())?;
        w.write_all(&crc32fast::hash(bytes).to_be_bytes())?;
        w.write_all(bytes)?;
        w.flush()?;
        self.written += RECORD_OVERHEAD + bytes.len() as u64;
        Ok(())
    }
}

#[derive(Deserialize)]
struct Seq {
    seq: u64,
}

/// Reads one record body into `buf`. `None` at the end of the file, on a torn record, on a bad
/// CRC or when the length can't be real (longer than the whole file).
fn read_record(r: &mut impl Read, buf: &mut Vec<u8>, max_len: u64) -> Option<usize> {
    let mut len = [0u8; 4];
    let mut crc = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    r.read_exact(&mut crc).ok()?;
    let len = u32::from_be_bytes(len) as usize;
    if len as u64 > max_len {
        return None;
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).ok()?;
    (crc32fast::hash(buf) == u32::from_be_bytes(crc)).then_some(len)
}
