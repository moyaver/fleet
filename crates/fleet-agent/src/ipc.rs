// SPDX-License-Identifier: AGPL-3.0-only
//! Framing between the supervisor and the observer (research 02 §1.6).
//!
//! Each frame is `[kind: u8][len: u32 big-endian][payload]`. Output chunks carry raw bytes;
//! control frames carry JSON, so the observer can evolve without the supervisor changing.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Largest payload accepted when decoding. Output chunks are at most 64 KiB.
const MAX_PAYLOAD: u32 = 1024 * 1024;

const KIND_STDOUT: u8 = 1;
const KIND_STDERR: u8 = 2;
const KIND_CONTROL: u8 = 3;

/// Which output stream a chunk or gap belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Stream {
    /// The child's standard output.
    Stdout,
    /// The child's standard error.
    Stderr,
}

/// Supervisor → observer control messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Control {
    /// The child is running.
    ChildStarted {
        /// Child PID.
        pid: u32,
        /// Child process group, when it got its own.
        pgid: Option<u32>,
        /// The command line, unredacted: the observer redacts before anything leaves the host.
        argv: Vec<String>,
        /// Supervisor wall clock at spawn, Unix milliseconds.
        started_at_ms: u64,
        /// Where the session log and flow bundles will be, when known.
        #[serde(default)]
        debug_dir: Option<String>,
        /// Where the JUnit report will be, when known.
        #[serde(default)]
        report: Option<String>,
        /// The agent's own directory for this run (spool, logs).
        #[serde(default)]
        run_dir: Option<String>,
        /// The Fleet plan shard this run executes, if any.
        #[serde(default)]
        plan: Option<fleet_protocol::event::PlanAssigned>,
    },
    /// The supervisor received a signal and forwarded it.
    Signal {
        /// Signal number.
        signal: i32,
    },
    /// Output copies were dropped because the observer fell behind (invariant I5).
    Gap {
        /// Stream the bytes were dropped from.
        stream: Stream,
        /// Number of bytes dropped.
        bytes: u64,
    },
    /// The child exited.
    ChildExited {
        /// Exit code, if it exited normally.
        code: Option<i32>,
        /// Signal number, if a signal killed it.
        signal: Option<i32>,
    },
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// A chunk of child output, exactly as read.
    Output(Stream, Vec<u8>),
    /// A control message.
    Control(Control),
}

impl Frame {
    /// Writes the frame. A single `write_all` keeps frames atomic on the pipe.
    ///
    /// # Errors
    /// Returns the writer's error, or `InvalidInput` if the payload is too large.
    pub(crate) fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let (kind, payload) = match self {
            Self::Output(Stream::Stdout, bytes) => (KIND_STDOUT, bytes.clone()),
            Self::Output(Stream::Stderr, bytes) => (KIND_STDERR, bytes.clone()),
            Self::Control(c) => (
                KIND_CONTROL,
                serde_json::to_vec(c).map_err(io::Error::other)?,
            ),
        };
        let len = u32::try_from(payload.len())
            .ok()
            .filter(|l| *l <= MAX_PAYLOAD)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        let mut buf = Vec::with_capacity(5 + payload.len());
        buf.push(kind);
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&payload);
        w.write_all(&buf)
    }

    /// Reads one frame; `Ok(None)` on a clean end of stream.
    ///
    /// # Errors
    /// Returns `InvalidData` for an unknown kind, oversized payload or bad control JSON, and
    /// `UnexpectedEof` for a truncated frame.
    pub(crate) fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let mut header = [0u8; 5];
        match r.read_exact(&mut header[..1]) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            other => other?,
        }
        r.read_exact(&mut header[1..])?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        let frame = match header[0] {
            KIND_STDOUT => Self::Output(Stream::Stdout, payload),
            KIND_STDERR => Self::Output(Stream::Stderr, payload),
            KIND_CONTROL => Self::Control(
                serde_json::from_slice(&payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame kind {other}"),
                ));
            }
        };
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Output(Stream::Stdout, b"\xff\x00partial".to_vec()),
            Frame::Output(Stream::Stderr, Vec::new()),
            Frame::Control(Control::ChildStarted {
                pid: 7,
                pgid: Some(7),
                argv: vec!["maestro".into()],
                started_at_ms: 1,
                debug_dir: Some("/run/artifacts".into()),
                report: None,
                run_dir: None,
                plan: None,
            }),
            Frame::Control(Control::Gap {
                stream: Stream::Stderr,
                bytes: 42,
            }),
            Frame::Control(Control::ChildExited {
                code: None,
                signal: Some(15),
            }),
        ];
        let mut buf = Vec::new();
        for f in &frames {
            f.write_to(&mut buf).unwrap();
        }
        let mut r = buf.as_slice();
        for f in &frames {
            assert_eq!(Frame::read_from(&mut r).unwrap().as_ref(), Some(f));
        }
        assert_eq!(Frame::read_from(&mut r).unwrap(), None);
    }

    #[test]
    fn rejects_corrupt_input() {
        assert!(
            Frame::read_from(&mut [9u8, 0, 0, 0, 0].as_slice()).is_err(),
            "unknown kind"
        );
        assert!(
            Frame::read_from(&mut [1u8, 0xff, 0xff, 0xff, 0xff].as_slice()).is_err(),
            "too large"
        );
        assert!(
            Frame::read_from(&mut [1u8, 0, 0, 0, 4, b'a'].as_slice()).is_err(),
            "truncated"
        );
        assert!(
            Frame::read_from(&mut [3u8, 0, 0, 0, 1, b'{'].as_slice()).is_err(),
            "bad json"
        );
    }
}
