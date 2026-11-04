// SPDX-License-Identifier: AGPL-3.0-only
//! The journal envelope (research 05 §3.2, §5.3, §5.4).
//!
//! Identity is `(agent_run_id, seq)`: `seq` starts at 1 and is gap-free within an agent run.
//! The body travels as `type` + `v` + `data`, so a server that doesn't know a type still stores
//! and acks it, and ignores it in projections (PLAN §8 compatibility rules).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::event::EventBody;

/// When something happened: the agent's wall clock and its monotonic clock since process start.
///
/// Durations come from `mono_ms` differences, never from wall-clock subtraction (research 05 §5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSample {
    /// Agent wall clock, UTC.
    #[serde(with = "time::serde::rfc3339")]
    pub wall: OffsetDateTime,
    /// Milliseconds since the agent process started, monotonic.
    pub mono_ms: u64,
}

/// Where a piece of evidence came from. The claim lattice ranks these (fleet-core).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Supplied up front: CLI arguments, a manifest.
    Declared,
    /// The agent's own direct observation: adb, signals, spawn, files existing.
    AgentObserved,
    /// Maestro's exit status.
    ProcessExit,
    /// Parsed Maestro stdout/stderr.
    LiveConsole,
    /// Parsed `maestro.log`.
    DebugLog,
    /// Structured per-flow artifacts such as `commands.json`.
    ArtifactFile,
    /// Maestro's final JUnit report.
    JunitFinal,
    /// Inferred by the server (for example after an agent disappeared). Agents may not send it.
    ServerInferred,
    /// A kind this build doesn't know. Never ranks as evidence of success.
    #[serde(other)]
    Unrecognized,
}

/// The provenance of one event (invariant I4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Where the evidence came from.
    pub kind: SourceKind,
    /// Our parser and the Maestro version profile it used, for example `maestro-log@2.12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Where in the source: a line number or a file path relative to the run directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

impl Provenance {
    /// A provenance with no parser or locator.
    #[must_use]
    pub fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            parser: None,
            locator: None,
        }
    }

    /// Sets the parser identifier.
    #[must_use]
    pub fn parser(mut self, parser: impl Into<String>) -> Self {
        self.parser = Some(parser.into());
        self
    }

    /// Sets the locator.
    #[must_use]
    pub fn locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }
}

/// Which execution entities an event is about. All IDs are agent-generated UUIDv7.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// One Maestro invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_execution_id: Option<Uuid>,
    /// One shard of that invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_id: Option<Uuid>,
    /// One device for the life of this agent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_session_id: Option<Uuid>,
    /// One execution of one flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_execution_id: Option<Uuid>,
    /// One step of that flow execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepRef>,
}

impl Subject {
    /// Whether no entity is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A step inside a flow execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    /// 0-based position in the flow's steps as they were observed (nested steps included).
    pub index: u32,
}

/// One journal event as it travels and is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Gap-free sequence number within the agent run, from 1.
    pub seq: u64,
    /// Event type, for example `flow.ended`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Version of this type's payload.
    pub v: u16,
    /// When it happened (not when it was sent).
    pub at: ClockSample,
    /// The entities it's about.
    #[serde(default, skip_serializing_if = "Subject::is_empty")]
    pub subject: Subject,
    /// Where the evidence came from.
    pub src: Provenance,
    /// The type-specific payload, kept as JSON so unknown types survive.
    pub data: serde_json::Value,
}

impl EventEnvelope {
    /// Wraps a typed body.
    #[must_use]
    pub fn new(
        seq: u64,
        at: ClockSample,
        subject: Subject,
        src: Provenance,
        body: &EventBody,
    ) -> Self {
        let (kind, data) = body.to_parts();
        Self {
            seq,
            kind,
            v: body.version(),
            at,
            subject,
            src,
            data,
        }
    }

    /// The typed body, or `None` for a type or version this build doesn't know.
    ///
    /// # Errors
    /// Returns an error when the type is known but the payload doesn't match it.
    pub fn decode(&self) -> Result<Option<EventBody>, serde_json::Error> {
        EventBody::from_parts(&self.kind, self.v, &self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FlowOutcome, RunSignal};

    fn at() -> ClockSample {
        ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 7,
        }
    }

    #[test]
    fn known_bodies_round_trip_through_the_envelope() {
        let body = EventBody::RunSignal(RunSignal { signal: 15 });
        let env = EventEnvelope::new(
            3,
            at(),
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &body,
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"run.signal""#), "{json}");
        assert!(
            !json.contains("subject"),
            "empty subject is omitted: {json}"
        );
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.decode().unwrap(), Some(body));
    }

    #[test]
    fn unknown_types_and_versions_survive_and_decode_to_none() {
        let json = r#"{"seq":9,"type":"future.thing","v":1,"at":{"wall":"2026-11-04T00:00:00Z","mono_ms":1},
                       "src":{"kind":"live_console"},"data":{"anything":[1,2]}}"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.decode().unwrap(), None);
        assert_eq!(
            serde_json::to_value(&env).unwrap()["data"]["anything"][1],
            2
        );

        let mut newer = EventEnvelope::new(
            1,
            at(),
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::RunSignal(RunSignal { signal: 2 }),
        );
        newer.v = 99;
        assert_eq!(
            newer.decode().unwrap(),
            None,
            "unknown version is not guessed at"
        );
    }

    #[test]
    fn a_known_type_with_a_bad_payload_is_an_error() {
        let mut env = EventEnvelope::new(
            1,
            at(),
            Subject::default(),
            Provenance::new(SourceKind::JunitFinal),
            &EventBody::RunSignal(RunSignal { signal: 2 }),
        );
        env.data = serde_json::json!({ "signal": "two" });
        assert!(env.decode().is_err());
    }

    #[test]
    fn unknown_source_kinds_are_unrecognized() {
        let p: Provenance = serde_json::from_str(r#"{"kind":"quantum_oracle"}"#).unwrap();
        assert_eq!(p.kind, SourceKind::Unrecognized);
        let o: FlowOutcome = serde_json::from_str(r#""half_passed""#).unwrap();
        assert_eq!(o, FlowOutcome::Unrecognized, "never read as passed");
    }
}
