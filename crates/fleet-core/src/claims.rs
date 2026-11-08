// SPDX-License-Identifier: AGPL-3.0-only
//! The claim lattice (research 05 §7; invariants I3, I4).
//!
//! A flow execution holds a set of claims. Its outcome is [`resolve`] of that set: a pure
//! function, so any delivery order and any number of duplicates give the same answer. Only a
//! claim of rank ≥ 30 (a parsed Maestro source) may claim `passed`; [`Claim::new`] refuses the
//! rest, so nothing is ever falsely green.

use fleet_protocol::{event::FlowOutcome, journal::SourceKind};

/// Precedence of a source. Higher wins.
#[must_use]
pub fn rank(source: SourceKind) -> u8 {
    match source {
        SourceKind::JunitFinal => 50,
        SourceKind::ArtifactFile => 40,
        SourceKind::LiveConsole | SourceKind::DebugLog => 30,
        SourceKind::ProcessExit | SourceKind::AgentObserved => 20,
        SourceKind::ServerInferred => 10,
        SourceKind::Declared | SourceKind::Unrecognized => 0,
    }
}

/// Whether `source` may claim `outcome` at all.
#[must_use]
pub fn may_claim(source: SourceKind, outcome: FlowOutcome) -> bool {
    use FlowOutcome::{Failed, Interrupted, Passed, Skipped, Unknown, Unrecognized};
    match source {
        SourceKind::JunitFinal => matches!(outcome, Passed | Failed | Interrupted | Unknown),
        SourceKind::ArtifactFile | SourceKind::LiveConsole | SourceKind::DebugLog => {
            matches!(outcome, Passed | Failed)
        }
        SourceKind::ProcessExit | SourceKind::AgentObserved | SourceKind::ServerInferred => {
            matches!(outcome, Interrupted | Unknown)
        }
        SourceKind::Declared => matches!(outcome, Skipped),
        SourceKind::Unrecognized => matches!(outcome, Unknown | Unrecognized),
    }
}

/// One source's claim about one flow execution's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Claim {
    source: SourceKind,
    outcome: FlowOutcome,
    /// Arrival order within the agent run (the event's `seq`); breaks same-rank ties.
    order: u64,
}

/// A claim that a source isn't allowed to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimRefused {
    /// The source.
    pub source: SourceKind,
    /// What it tried to claim.
    pub outcome: FlowOutcome,
}

impl std::fmt::Display for ClaimRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} may not claim {:?}", self.source, self.outcome)
    }
}

impl std::error::Error for ClaimRefused {}

impl Claim {
    /// A validated claim. An unrecognized outcome is stored as `unknown`.
    ///
    /// # Errors
    /// Returns [`ClaimRefused`] when `source` may not claim `outcome` (for example a process
    /// exit claiming `passed`).
    pub fn new(source: SourceKind, outcome: FlowOutcome, order: u64) -> Result<Self, ClaimRefused> {
        if !may_claim(source, outcome) {
            return Err(ClaimRefused { source, outcome });
        }
        let outcome = if outcome == FlowOutcome::Unrecognized {
            FlowOutcome::Unknown
        } else {
            outcome
        };
        Ok(Self {
            source,
            outcome,
            order,
        })
    }

    /// The source.
    #[must_use]
    pub fn source(&self) -> SourceKind {
        self.source
    }

    /// The claimed outcome.
    #[must_use]
    pub fn outcome(&self) -> FlowOutcome {
        self.outcome
    }
}

/// How settled a resolved outcome is (spec §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// From a live parser while the suite is still running.
    Provisional,
    /// From the final report or per-flow artifacts.
    Verified,
    /// From a live parser, the suite has ended, and no report confirmed it.
    UnverifiedFinal,
    /// From an observation that can only say interrupted or unknown.
    Inferred,
}

/// Where a flow execution stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowState {
    /// Planned, not started.
    Pending,
    /// Started, no terminal claim yet.
    Running,
    /// A terminal outcome.
    Ended(FlowOutcome),
}

/// The result of [`resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// Where the flow stands.
    pub state: FlowState,
    /// How settled that is.
    pub finality: Finality,
    /// The source of the winning claim.
    pub source: Option<SourceKind>,
    /// Two parsed sources (rank ≥ 30) disagreed on passed/failed: a Maestro compatibility alarm.
    pub conflict: bool,
}

/// Resolves a set of claims. Order and duplicates don't matter.
///
/// `run_ended` says the run's evidence is complete: its suite closed, or its agent run ended. A
/// started flow with no claim at that point has no verdict and never will (no source records one),
/// so it is `unknown` and inferred, not running. A flow that is not started is pending.
#[must_use]
pub fn resolve(claims: &[Claim], started: bool, run_ended: bool) -> Resolution {
    let Some(top) = claims.iter().map(|c| rank(c.source)).max() else {
        let (state, finality) = match (started, run_ended) {
            (false, _) => (FlowState::Pending, Finality::Provisional),
            (true, false) => (FlowState::Running, Finality::Provisional),
            (true, true) => (FlowState::Ended(FlowOutcome::Unknown), Finality::Inferred),
        };
        return Resolution {
            state,
            finality,
            source: None,
            conflict: false,
        };
    };
    // Same rank: the latest claim wins; ties on order break by (source, outcome) so the result
    // never depends on input order.
    let winner = claims
        .iter()
        .filter(|c| rank(c.source) == top)
        .max_by_key(|c| (c.order, c.source, c.outcome))
        .copied()
        .unwrap_or(Claim {
            source: SourceKind::Unrecognized,
            outcome: FlowOutcome::Unknown,
            order: 0,
        });
    let conflict = claims.iter().any(|c| {
        rank(c.source) >= 30
            && matches!(c.outcome, FlowOutcome::Passed | FlowOutcome::Failed)
            && c.outcome != winner.outcome
    });
    let finality = match top {
        40.. => Finality::Verified,
        30..40 if run_ended => Finality::UnverifiedFinal,
        30..40 => Finality::Provisional,
        _ => Finality::Inferred,
    };
    Resolution {
        state: FlowState::Ended(winner.outcome),
        finality,
        source: Some(winner.source),
        conflict,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn claim(source: SourceKind, outcome: FlowOutcome, order: u64) -> Claim {
        Claim::new(source, outcome, order).unwrap()
    }

    #[test]
    fn only_parsed_sources_may_claim_passed() {
        for s in [
            SourceKind::ProcessExit,
            SourceKind::AgentObserved,
            SourceKind::ServerInferred,
            SourceKind::Declared,
            SourceKind::Unrecognized,
        ] {
            assert!(Claim::new(s, FlowOutcome::Passed, 1).is_err(), "{s:?}");
        }
        for s in [
            SourceKind::LiveConsole,
            SourceKind::DebugLog,
            SourceKind::ArtifactFile,
            SourceKind::JunitFinal,
        ] {
            assert!(Claim::new(s, FlowOutcome::Passed, 1).is_ok(), "{s:?}");
        }
    }

    #[test]
    fn no_claims_means_pending_or_running() {
        assert_eq!(resolve(&[], false, false).state, FlowState::Pending);
        assert_eq!(resolve(&[], true, false).state, FlowState::Running);
    }

    /// A started flow that never got a verdict, once its run ended, is unknown and inferred, with no
    /// source: it is not running, and nothing says it passed or failed (I3).
    #[test]
    fn a_started_flow_with_no_verdict_after_its_run_ended_is_unknown_and_inferred() {
        let r = resolve(&[], true, true);
        assert_eq!(r.state, FlowState::Ended(FlowOutcome::Unknown));
        assert_eq!(r.finality, Finality::Inferred);
        assert_eq!(r.source, None);
        assert!(!r.conflict);
    }

    #[test]
    fn junit_overrides_live_and_flags_the_disagreement() {
        let r = resolve(
            &[
                claim(SourceKind::LiveConsole, FlowOutcome::Failed, 1),
                claim(SourceKind::JunitFinal, FlowOutcome::Passed, 2),
            ],
            true,
            true,
        );
        assert_eq!(r.state, FlowState::Ended(FlowOutcome::Passed));
        assert_eq!(r.finality, Finality::Verified);
        assert!(r.conflict);
    }

    #[test]
    fn live_results_are_provisional_then_unverified() {
        let c = [claim(SourceKind::LiveConsole, FlowOutcome::Passed, 1)];
        assert_eq!(resolve(&c, true, false).finality, Finality::Provisional);
        assert_eq!(resolve(&c, true, true).finality, Finality::UnverifiedFinal);
    }

    #[test]
    fn interruption_is_inferred_and_late_evidence_wins() {
        let interrupted = claim(SourceKind::ProcessExit, FlowOutcome::Interrupted, 5);
        let r = resolve(&[interrupted], true, true);
        assert_eq!(
            (r.state, r.finality),
            (
                FlowState::Ended(FlowOutcome::Interrupted),
                Finality::Inferred
            )
        );
        let r = resolve(
            &[
                interrupted,
                claim(SourceKind::LiveConsole, FlowOutcome::Failed, 3),
            ],
            true,
            true,
        );
        assert_eq!(
            r.state,
            FlowState::Ended(FlowOutcome::Failed),
            "a parsed result outranks an inference"
        );
    }

    /// Only claims a source may make. The allowed (source, outcome) pairs are enumerated once and
    /// sampled directly, so no case is rejected: a filter would hit proptest's local-reject limit
    /// at high `PROPTEST_CASES`.
    fn any_claim() -> impl Strategy<Value = Claim> {
        let sources = [
            SourceKind::Declared,
            SourceKind::AgentObserved,
            SourceKind::ProcessExit,
            SourceKind::LiveConsole,
            SourceKind::DebugLog,
            SourceKind::ArtifactFile,
            SourceKind::JunitFinal,
            SourceKind::ServerInferred,
        ];
        let outcomes = [
            FlowOutcome::Passed,
            FlowOutcome::Failed,
            FlowOutcome::Skipped,
            FlowOutcome::Interrupted,
            FlowOutcome::Unknown,
        ];
        let allowed: Vec<(SourceKind, FlowOutcome)> = sources
            .into_iter()
            .flat_map(|s| outcomes.into_iter().map(move |o| (s, o)))
            .filter(|&(s, o)| may_claim(s, o))
            .collect();
        (prop::sample::select(allowed), 0u64..20)
            .prop_map(|((s, o), n)| Claim::new(s, o, n).expect("pairs were filtered by may_claim"))
    }

    proptest! {
        #[test]
        fn order_and_duplicates_never_matter(claims in prop::collection::vec(any_claim(), 0..12), seed in any::<u64>()) {
            let mut shuffled = claims.clone();
            // Deterministic shuffle plus duplicates.
            shuffled.extend(claims.iter().take(3));
            let n = shuffled.len().max(1);
            shuffled.rotate_left(usize::try_from(seed % n as u64).unwrap());
            shuffled.reverse();
            prop_assert_eq!(resolve(&claims, true, true), resolve(&shuffled, true, true));
        }

        #[test]
        fn passed_needs_a_parsed_source(claims in prop::collection::vec(any_claim(), 1..12), closed in any::<bool>()) {
            let r = resolve(&claims, true, closed);
            if r.state == FlowState::Ended(FlowOutcome::Passed) {
                prop_assert!(r.source.is_some_and(|s| rank(s) >= 30));
                prop_assert_ne!(r.finality, Finality::Inferred);
            }
        }

        #[test]
        fn a_final_report_always_decides(claims in prop::collection::vec(any_claim(), 0..12), outcome in prop::sample::select(vec![FlowOutcome::Passed, FlowOutcome::Failed, FlowOutcome::Interrupted])) {
            let mut all = claims;
            all.push(claim(SourceKind::JunitFinal, outcome, 100));
            let r = resolve(&all, true, true);
            prop_assert_eq!(r.state, FlowState::Ended(outcome));
            prop_assert_eq!(r.finality, Finality::Verified);
        }
    }
}
