// SPDX-License-Identifier: AGPL-3.0-only
//! Spots a missed change on one socket: a delta that skips a version.
//!
//! The server sends an attempt's deltas in version order, but after a short wait it sends a later
//! version anyway if an earlier one never went out, and leaves the client to see the gap and
//! resync (`fleet-server`'s live hub). The live store notices the gap too and stops applying
//! deltas until a snapshot arrives, but only the socket can ask for one: it must open a new
//! subscription, so it watches the versions itself, with the store's own rule.

use std::collections::HashMap;

use fleet_protocol::live::LiveFrame;
use uuid::Uuid;

/// Whether a frame keeps the versions of its attempt contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contiguity {
    /// Nothing was missed (or the frame says nothing about versions).
    Fine,
    /// A version was skipped: take a new snapshot.
    Gap,
}

/// The last version seen for every attempt this socket sent a snapshot of.
#[derive(Debug, Default)]
pub struct VersionWatch {
    last: HashMap<Uuid, u64>,
}

impl VersionWatch {
    /// Looks at the next frame from the socket. A delta is fine when it is the next version, or
    /// one already seen (the store ignores it); it is a gap when it is further ahead. Deltas of
    /// an attempt this socket sent no snapshot of are none of its business.
    pub fn observe(&mut self, frame: &LiveFrame) -> Contiguity {
        match frame {
            LiveFrame::Snapshot { snapshot } => {
                let version = u64::try_from(snapshot.attempt.version).unwrap_or(0);
                self.last.insert(snapshot.attempt.id, version);
                Contiguity::Fine
            }
            LiveFrame::Delta {
                attempt_id,
                version,
                ..
            } => match self.last.get_mut(attempt_id) {
                Some(last) if *version == *last + 1 => {
                    *last = *version;
                    Contiguity::Fine
                }
                Some(last) if *version > *last => Contiguity::Gap,
                _ => Contiguity::Fine,
            },
            _ => Contiguity::Fine,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ATTEMPT: u128 = 1;

    fn snapshot(attempt: u128, version: u64) -> LiveFrame {
        serde_json::from_value(json!({
            "op": "snapshot",
            "snapshot": {
                "attempt": {
                    "id": Uuid::from_u128(attempt), "project_id": Uuid::from_u128(100),
                    "provider": "generic", "run_key": "acme/app:1", "repository": null,
                    "commit_sha": null, "git_ref": null, "attempt": 1, "version": version,
                    "planned_total": null, "jobs": 0, "open_jobs": 0,
                    "first_seen_at": "2026-01-01T00:00:00Z"
                },
                "jobs": [], "agent_runs": [], "suites": [], "devices": [], "flows": [],
                "plans": []
            }
        }))
        .unwrap()
    }

    fn delta(attempt: u128, version: u64) -> LiveFrame {
        LiveFrame::Delta {
            project_id: Uuid::from_u128(100),
            attempt_id: Uuid::from_u128(attempt),
            version,
            entities: vec![],
        }
    }

    #[test]
    fn deltas_that_follow_the_snapshot_one_by_one_are_fine() {
        let mut w = VersionWatch::default();
        assert_eq!(w.observe(&snapshot(ATTEMPT, 5)), Contiguity::Fine);
        assert_eq!(w.observe(&delta(ATTEMPT, 6)), Contiguity::Fine);
        assert_eq!(w.observe(&delta(ATTEMPT, 7)), Contiguity::Fine);
    }

    #[test]
    fn a_skipped_version_is_a_gap() {
        let mut w = VersionWatch::default();
        let _ = w.observe(&snapshot(ATTEMPT, 5));
        assert_eq!(w.observe(&delta(ATTEMPT, 6)), Contiguity::Fine);
        assert_eq!(
            w.observe(&delta(ATTEMPT, 8)),
            Contiguity::Gap,
            "7 never arrived"
        );
    }

    #[test]
    fn a_delta_the_snapshot_already_covers_is_not_a_gap() {
        let mut w = VersionWatch::default();
        let _ = w.observe(&snapshot(ATTEMPT, 5));
        assert_eq!(w.observe(&delta(ATTEMPT, 5)), Contiguity::Fine);
        assert_eq!(w.observe(&delta(ATTEMPT, 3)), Contiguity::Fine);
        assert_eq!(w.observe(&delta(ATTEMPT, 6)), Contiguity::Fine);
    }

    #[test]
    fn attempts_are_followed_separately_and_unseen_ones_are_ignored() {
        let mut w = VersionWatch::default();
        let _ = w.observe(&snapshot(1, 5));
        let _ = w.observe(&snapshot(2, 40));
        assert_eq!(w.observe(&delta(2, 41)), Contiguity::Fine);
        assert_eq!(w.observe(&delta(1, 6)), Contiguity::Fine);
        assert_eq!(
            w.observe(&delta(3, 900)),
            Contiguity::Fine,
            "no snapshot of attempt 3 on this socket"
        );
    }

    #[test]
    fn a_new_snapshot_starts_the_count_again() {
        let mut w = VersionWatch::default();
        let _ = w.observe(&snapshot(ATTEMPT, 5));
        assert_eq!(w.observe(&delta(ATTEMPT, 9)), Contiguity::Gap);
        let _ = w.observe(&snapshot(ATTEMPT, 9));
        assert_eq!(w.observe(&delta(ATTEMPT, 10)), Contiguity::Fine);
    }
}
