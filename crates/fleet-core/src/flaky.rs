// SPDX-License-Identifier: AGPL-3.0-only
//! Flaky v1 (ADR-0009): a pure classification over a flow's run history.
//!
//! - **Confirmed**: the same flow passed and failed on the same commit under the same condition
//!   fingerprint.
//! - **Suspected**: the flip rate over the last 20 default-branch runs under one condition
//!   fingerprint is at least 0.2. The rate is flips between consecutive runs divided by the
//!   number of consecutive pairs.
//! - **Excluded**: failures marked as infrastructure failures or as happening during a host
//!   anomaly never count.
//!
//! Only outcomes read directly from Maestro (a parsed source, final or verified) count; an
//! inferred outcome is never a pass (invariant I3). Every classification names the runs behind
//! it, so the dashboard can show why.

use std::collections::BTreeMap;

use fleet_protocol::{event::FlowOutcome, journal::SourceKind};
use uuid::Uuid;

use crate::{
    claims::{self, Finality},
    fingerprint::ConditionFingerprint,
};

/// Why a failed run is left out of flakiness (ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exclusion {
    /// The failure was classified as infrastructure (device lost, emulator boot failure).
    Infrastructure,
    /// The host was in an anomaly (resource exhaustion) while the flow failed.
    HostAnomaly,
}

/// The exception Maestro raises when the device's driver server dies mid-flow (fixture
/// `device-killed`): the failure is the device's, not the flow's.
pub const DEVICE_LOST: &str = "maestro.android.DeviceServerDiedException";

/// The agent's `agent.warning` code when a flow's `commands.json` is not final: the flow was cut
/// short, which is what a lost device leaves behind (the recorded half of the device-loss
/// evidence, issue #145).
pub const CUT_SHORT: &str = "commands_not_final";

/// The exclusion a stored run carries, judged from its outcome and failure message (ADR-0009).
///
/// Only a failure can be excluded, and only one whose message names a lost device counts as
/// infrastructure. This reads what ingest already stores, so it needs no schema.
#[must_use]
pub fn exclusion_of(outcome: FlowOutcome, failure_message: Option<&str>) -> Option<Exclusion> {
    let lost_device = failure_message.is_some_and(|m| m.contains(DEVICE_LOST));
    (outcome == FlowOutcome::Failed && lost_device).then_some(Exclusion::Infrastructure)
}

/// One finished flow execution, as the history holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRun {
    /// The flow execution; named as evidence.
    pub run: Uuid,
    /// The flow's identity (its repository path when known, else its name).
    pub flow: String,
    /// The commit that was tested; `None` when the CI didn't say.
    pub commit: Option<String>,
    /// The conditions it ran under.
    pub condition: ConditionFingerprint,
    /// The resolved outcome.
    pub outcome: FlowOutcome,
    /// The source that decided the outcome.
    pub source: SourceKind,
    /// How settled the outcome is.
    pub finality: Finality,
    /// Whether the run was on the project's default branch.
    pub default_branch: bool,
    /// When the flow ended, in Unix milliseconds.
    pub at_unix_ms: i64,
    /// Set on failures that must not count.
    pub exclusion: Option<Exclusion>,
}

/// The tunables of flaky v1; [`Default`] is what ADR-0009 states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlakyConfig {
    /// How many of the latest default-branch runs the flip rate looks at.
    pub window: usize,
    /// The flip rate at or above which a flow is suspected, as `numerator / denominator`.
    pub threshold: (u32, u32),
    /// Fewer counted runs than this never make a flow suspected.
    pub min_runs: usize,
}

impl Default for FlakyConfig {
    fn default() -> Self {
        Self {
            window: 20,
            threshold: (1, 5),
            min_runs: 5,
        }
    }
}

/// How flaky a flow is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Flakiness {
    /// Nothing in the history says it is.
    NotFlaky,
    /// Its recent default-branch results flip often.
    Suspected,
    /// It passed and failed on one commit under one condition.
    Confirmed,
}

/// The runs behind a classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// One commit and condition with both outcomes.
    PassedAndFailed {
        /// The commit.
        commit: String,
        /// The conditions.
        condition: ConditionFingerprint,
        /// The passing runs, oldest first.
        passed: Vec<Uuid>,
        /// The failing runs, oldest first.
        failed: Vec<Uuid>,
    },
    /// A flip rate over the latest default-branch runs of one condition.
    Flips {
        /// The conditions.
        condition: ConditionFingerprint,
        /// The counted runs in the window, oldest first.
        runs: Vec<Uuid>,
        /// How many consecutive runs differ in outcome.
        flips: u32,
    },
}

impl Evidence {
    /// The flip rate of [`Evidence::Flips`]: flips over consecutive pairs.
    #[must_use]
    pub fn flip_rate(&self) -> Option<f64> {
        match self {
            Self::Flips { runs, flips, .. } => {
                let pairs = runs.len().saturating_sub(1);
                (pairs > 0).then(|| {
                    f64::from(*flips) / f64::from(u32::try_from(pairs).unwrap_or(u32::MAX))
                })
            }
            Self::PassedAndFailed { .. } => None,
        }
    }
}

/// One flow's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowFlakiness {
    /// The flow's identity.
    pub flow: String,
    /// The verdict.
    pub flakiness: Flakiness,
    /// What supports it; empty for [`Flakiness::NotFlaky`].
    pub evidence: Vec<Evidence>,
    /// How many of its failed runs were excluded (ADR-0009), so they never counted.
    pub excluded: usize,
}

/// Classifies every flow in `history`, in flow order. Input order doesn't matter.
#[must_use]
pub fn classify(history: &[HistoryRun], config: &FlakyConfig) -> Vec<FlowFlakiness> {
    let mut by_flow: BTreeMap<&str, Vec<&HistoryRun>> = BTreeMap::new();
    for run in history {
        by_flow.entry(run.flow.as_str()).or_default().push(run);
    }
    by_flow
        .into_iter()
        .map(|(flow, mut runs)| {
            runs.sort_by_key(|r| (r.at_unix_ms, r.run));
            let excluded = runs.iter().filter(|r| excluded_failure(r)).count();
            runs.retain(|r| counts(r));
            let mut evidence = confirmed(&runs);
            let confirmed = !evidence.is_empty();
            let suspected = suspected(&runs, config);
            let flakiness = if confirmed {
                Flakiness::Confirmed
            } else if !suspected.is_empty() {
                Flakiness::Suspected
            } else {
                Flakiness::NotFlaky
            };
            evidence.extend(suspected);
            FlowFlakiness {
                flow: flow.to_owned(),
                flakiness,
                evidence,
                excluded,
            }
        })
        .collect()
}

/// Whether a run is a direct, settled observation of a pass or a failure, exclusion aside.
///
/// A pass needs a source that may claim it (invariant I3): inferred, exit-status and declared
/// sources never do. Both outcomes need a parsed Maestro source and a final result.
fn observed(r: &HistoryRun) -> bool {
    matches!(r.finality, Finality::Verified | Finality::UnverifiedFinal)
        && matches!(r.outcome, FlowOutcome::Passed | FlowOutcome::Failed)
        && claims::may_claim(r.source, r.outcome)
        && claims::rank(r.source) >= PARSED_RANK
}

/// Whether a run is a failure that would count but for its exclusion.
fn excluded_failure(r: &HistoryRun) -> bool {
    observed(r) && r.outcome == FlowOutcome::Failed && r.exclusion.is_some()
}

/// Whether a run counts toward flakiness: observed, and not an excluded failure.
fn counts(r: &HistoryRun) -> bool {
    observed(r) && !excluded_failure(r)
}

/// The lowest rank of a parsed Maestro source (`claims::rank`: live console and debug log).
const PARSED_RANK: u8 = 30;

/// Commits and conditions where the flow both passed and failed.
fn confirmed(runs: &[&HistoryRun]) -> Vec<Evidence> {
    type Key<'a> = (&'a str, ConditionFingerprint);
    let mut groups: BTreeMap<Key<'_>, (Vec<Uuid>, Vec<Uuid>)> = BTreeMap::new();
    for r in runs {
        let Some(commit) = r.commit.as_deref().filter(|c| !c.is_empty()) else {
            continue;
        };
        let (passed, failed) = groups.entry((commit, r.condition)).or_default();
        if r.outcome == FlowOutcome::Passed {
            passed.push(r.run);
        } else {
            failed.push(r.run);
        }
    }
    groups
        .into_iter()
        .filter(|(_, (passed, failed))| !passed.is_empty() && !failed.is_empty())
        .map(
            |((commit, condition), (passed, failed))| Evidence::PassedAndFailed {
                commit: commit.to_owned(),
                condition,
                passed,
                failed,
            },
        )
        .collect()
}

/// Conditions whose latest default-branch runs flip at or above the threshold.
fn suspected(runs: &[&HistoryRun], config: &FlakyConfig) -> Vec<Evidence> {
    let mut by_condition: BTreeMap<ConditionFingerprint, Vec<&HistoryRun>> = BTreeMap::new();
    for r in runs.iter().filter(|r| r.default_branch) {
        by_condition.entry(r.condition).or_default().push(r);
    }
    let (num, den) = config.threshold;
    by_condition
        .into_iter()
        .filter_map(|(condition, all)| {
            let window = all.get(all.len().saturating_sub(config.window)..)?;
            let pairs = window.len().checked_sub(1).filter(|p| *p > 0)?;
            if window.len() < config.min_runs {
                return None;
            }
            let flips = window
                .windows(2)
                .filter(|w| matches!(w, [a, b] if a.outcome != b.outcome))
                .count();
            // flips / pairs >= num / den, without floating point.
            let flaky = u128::try_from(flips).ok()? * u128::from(den)
                >= u128::from(num) * u128::try_from(pairs).ok()?;
            flaky.then(|| Evidence::Flips {
                condition,
                runs: window.iter().map(|r| r.run).collect(),
                flips: u32::try_from(flips).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(n: u8) -> ConditionFingerprint {
        crate::fingerprint::ConditionFacts {
            device_api_level: Some(u32::from(n)),
            ..Default::default()
        }
        .fingerprint()
    }

    /// A run that counts: read from a parsed final report.
    fn run(n: u128, flow: &str, commit: &str, outcome: FlowOutcome) -> HistoryRun {
        HistoryRun {
            run: Uuid::from_u128(n),
            flow: flow.into(),
            commit: Some(commit.into()),
            condition: cond(35),
            outcome,
            source: SourceKind::JunitFinal,
            finality: Finality::Verified,
            default_branch: false,
            at_unix_ms: i64::try_from(n).unwrap() * 1000,
            exclusion: None,
        }
    }

    fn verdict(history: &[HistoryRun], flow: &str) -> FlowFlakiness {
        classify(history, &FlakyConfig::default())
            .into_iter()
            .find(|f| f.flow == flow)
            .unwrap_or_else(|| panic!("no verdict for {flow}"))
    }

    fn flakiness(history: &[HistoryRun], flow: &str) -> Flakiness {
        verdict(history, flow).flakiness
    }

    #[test]
    fn a_pass_and_a_fail_on_one_commit_and_condition_is_confirmed_with_both_runs() {
        let h = [
            run(1, "login", "abc", FlowOutcome::Passed),
            run(2, "login", "abc", FlowOutcome::Failed),
        ];
        let v = verdict(&h, "login");
        assert_eq!(v.flakiness, Flakiness::Confirmed);
        assert_eq!(
            v.evidence,
            vec![Evidence::PassedAndFailed {
                commit: "abc".into(),
                condition: cond(35),
                passed: vec![Uuid::from_u128(1)],
                failed: vec![Uuid::from_u128(2)],
            }]
        );
    }

    #[test]
    fn a_failure_after_a_pass_on_a_later_commit_is_not_flaky() {
        let h = [
            run(1, "login", "abc", FlowOutcome::Passed),
            run(2, "login", "def", FlowOutcome::Failed),
        ];
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn the_same_commit_under_other_conditions_is_not_comparable() {
        let mut other = run(2, "login", "abc", FlowOutcome::Failed);
        other.condition = cond(36);
        let h = [run(1, "login", "abc", FlowOutcome::Passed), other];
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn a_run_without_a_commit_cannot_confirm_anything() {
        let mut a = run(1, "login", "abc", FlowOutcome::Passed);
        let mut b = run(2, "login", "abc", FlowOutcome::Failed);
        a.commit = None;
        b.commit = None;
        assert_eq!(flakiness(&[a, b], "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn an_inferred_outcome_never_counts_as_a_pass() {
        let mut inferred = run(1, "login", "abc", FlowOutcome::Passed);
        inferred.source = SourceKind::ServerInferred;
        inferred.finality = Finality::Inferred;
        let mut by_exit = run(3, "login", "abc", FlowOutcome::Passed);
        by_exit.source = SourceKind::ProcessExit;
        let h = [
            inferred,
            by_exit,
            run(2, "login", "abc", FlowOutcome::Failed),
        ];
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn provisional_interrupted_and_unknown_results_do_not_count() {
        let mut provisional = run(1, "login", "abc", FlowOutcome::Passed);
        provisional.finality = Finality::Provisional;
        let h = [
            provisional,
            run(2, "login", "abc", FlowOutcome::Failed),
            run(3, "login", "abc", FlowOutcome::Interrupted),
            run(4, "login", "abc", FlowOutcome::Unknown),
        ];
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn a_live_console_pass_with_no_report_still_counts() {
        let mut live = run(1, "login", "abc", FlowOutcome::Passed);
        live.source = SourceKind::LiveConsole;
        live.finality = Finality::UnverifiedFinal;
        let h = [live, run(2, "login", "abc", FlowOutcome::Failed)];
        assert_eq!(flakiness(&h, "login"), Flakiness::Confirmed);
    }

    #[test]
    fn infrastructure_failures_and_failures_during_host_anomalies_are_excluded() {
        for exclusion in [Exclusion::Infrastructure, Exclusion::HostAnomaly] {
            let mut failed = run(2, "login", "abc", FlowOutcome::Failed);
            failed.exclusion = Some(exclusion);
            let h = [run(1, "login", "abc", FlowOutcome::Passed), failed];
            assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky, "{exclusion:?}");
        }
    }

    /// The `<failure>` text of a captured Maestro 2.12.1 JUnit report.
    fn failure_text(report: &str) -> &str {
        let (_, rest) = report.split_once("<failure>").unwrap();
        rest.split_once("</failure>").unwrap().0
    }

    const DEVICE_KILLED: &str =
        include_str!("../../../fixtures/maestro/2.12.1/device-killed/run/report.xml");
    const FAIL: &str = include_str!("../../../fixtures/maestro/2.12.1/fail/run/report.xml");

    #[test]
    fn a_failure_of_a_lost_device_is_infrastructure_and_excluded() {
        assert_eq!(
            exclusion_of(FlowOutcome::Failed, Some(failure_text(DEVICE_KILLED))),
            Some(Exclusion::Infrastructure)
        );
    }

    #[test]
    fn an_assertion_failure_is_the_flows_own_and_is_not_excluded() {
        assert_eq!(
            exclusion_of(FlowOutcome::Failed, Some(failure_text(FAIL))),
            None
        );
    }

    #[test]
    fn only_a_failure_with_a_lost_device_message_is_excluded() {
        assert_eq!(
            exclusion_of(FlowOutcome::Passed, Some(failure_text(DEVICE_KILLED))),
            None
        );
        assert_eq!(exclusion_of(FlowOutcome::Failed, None), None);
    }

    #[test]
    fn excluded_failures_are_counted_per_flow_and_only_when_they_would_have_counted() {
        let mut lost = run(2, "login", "abc", FlowOutcome::Failed);
        lost.exclusion = Some(Exclusion::Infrastructure);
        // A lost device on a run that is not settled would not have counted anyway.
        let mut provisional = run(3, "login", "abc", FlowOutcome::Failed);
        provisional.finality = Finality::Provisional;
        provisional.exclusion = Some(Exclusion::Infrastructure);
        let mut other = run(4, "checkout", "abc", FlowOutcome::Failed);
        other.exclusion = Some(Exclusion::Infrastructure);
        let h = [
            run(1, "login", "abc", FlowOutcome::Passed),
            lost,
            provisional,
            run(5, "checkout", "abc", FlowOutcome::Passed),
            other,
            run(6, "checkout", "def", FlowOutcome::Failed),
        ];
        let login = verdict(&h, "login");
        assert_eq!((login.excluded, login.flakiness), (1, Flakiness::NotFlaky));
        let checkout = verdict(&h, "checkout");
        assert_eq!(checkout.excluded, 1);
        // A failure on another commit is neither excluded nor a confirmed flake.
        assert_eq!(checkout.flakiness, Flakiness::NotFlaky);
        let solo = [run(7, "solo", "abc", FlowOutcome::Passed)];
        assert_eq!(verdict(&solo, "solo").excluded, 0);
    }

    #[test]
    fn a_lost_device_failure_does_not_make_its_flow_flaky() {
        let mut lost = run(2, "login", "abc", FlowOutcome::Failed);
        // Without the exclusion the same pair is confirmed flaky.
        let h = [run(1, "login", "abc", FlowOutcome::Passed), lost.clone()];
        assert_eq!(flakiness(&h, "login"), Flakiness::Confirmed);
        lost.exclusion = exclusion_of(lost.outcome, Some(failure_text(DEVICE_KILLED)));
        let h = [run(1, "login", "abc", FlowOutcome::Passed), lost];
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    /// Default-branch runs under one condition, oldest first, from a `P`/`F` string.
    fn trail(flow: &str, outcomes: &str, first: u128) -> Vec<HistoryRun> {
        outcomes
            .chars()
            .zip(first..)
            .map(|(c, n)| {
                let outcome = if c == 'P' {
                    FlowOutcome::Passed
                } else {
                    FlowOutcome::Failed
                };
                let mut r = run(n, flow, &format!("c{n}"), outcome);
                r.default_branch = true;
                r
            })
            .collect()
    }

    #[test]
    fn suspected_flakiness_requires_five_eligible_default_branch_runs() {
        let mut history = trail("login", "PPPF", 1);
        let mut excluded = run(5, "login", "c5", FlowOutcome::Failed);
        excluded.default_branch = true;
        excluded.exclusion = Some(Exclusion::Infrastructure);
        history.push(excluded);
        assert_eq!(flakiness(&history, "login"), Flakiness::NotFlaky);

        let mut eligible = run(6, "login", "c6", FlowOutcome::Failed);
        eligible.default_branch = true;
        history.push(eligible);
        let verdict = verdict(&history, "login");
        assert_eq!(verdict.flakiness, Flakiness::Suspected);
        let [Evidence::Flips { flips, runs, .. }] = verdict.evidence.as_slice() else {
            panic!("expected one flip evidence, got {:?}", verdict.evidence);
        };
        assert_eq!((*flips, runs.len()), (1, 5));
    }

    #[test]
    fn a_flip_rate_of_exactly_one_fifth_is_suspected() {
        // 16 runs, 15 consecutive pairs, 3 flips (P→F, F→P, P→F): 3/15 = 0.2.
        let h = trail("login", "PPPPFPPPPPPFFFFF", 1);
        assert_eq!(flakiness(&h, "login"), Flakiness::Suspected);
        let v = verdict(&h, "login");
        let [e @ Evidence::Flips { flips, runs, .. }] = v.evidence.as_slice() else {
            panic!("expected one flip evidence, got {:?}", v.evidence);
        };
        assert_eq!((*flips, runs.len()), (3, 16));
        assert!((e.flip_rate().unwrap() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn a_flip_rate_below_one_fifth_is_not_flaky() {
        // 16 runs, 2 flips: 2/15.
        let h = trail("login", "PPPPPFFPPPPPPPPP", 1);
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn a_single_regression_on_main_is_not_flaky_once_there_is_a_history() {
        let h = trail("login", &format!("{}{}", "P".repeat(15), "FFFFF"), 1);
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn only_the_latest_twenty_default_branch_runs_are_looked_at() {
        // Ten flips long ago, then twenty stable passes.
        let h = trail("login", &format!("{}{}", "PF".repeat(5), "P".repeat(20)), 1);
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
        // The same flips inside the window.
        let h = trail("login", &format!("{}{}", "P".repeat(10), "PF".repeat(5)), 1);
        assert_eq!(flakiness(&h, "login"), Flakiness::Suspected);
    }

    #[test]
    fn runs_off_the_default_branch_do_not_feed_the_flip_rate() {
        let mut h = trail("login", &"PF".repeat(8), 1);
        for r in &mut h {
            r.default_branch = false;
        }
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn flips_are_counted_per_condition_not_across_them() {
        // Passes on API 35, failures on API 36: each condition is steady.
        let mut h = trail("login", &"PF".repeat(8), 1);
        for (i, r) in h.iter_mut().enumerate() {
            r.condition = cond(if i % 2 == 0 { 35 } else { 36 });
        }
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn excluded_failures_do_not_flip_the_rate() {
        let mut h = trail("login", &"PF".repeat(8), 1);
        for r in h.iter_mut().filter(|r| r.outcome == FlowOutcome::Failed) {
            r.exclusion = Some(Exclusion::HostAnomaly);
        }
        assert_eq!(flakiness(&h, "login"), Flakiness::NotFlaky);
    }

    #[test]
    fn every_flow_gets_its_own_verdict_in_flow_order() {
        let mut h = vec![
            run(1, "b", "abc", FlowOutcome::Passed),
            run(2, "b", "abc", FlowOutcome::Failed),
            run(3, "a", "abc", FlowOutcome::Passed),
        ];
        h.extend(trail("c", &"PF".repeat(8), 10));
        let all: Vec<_> = classify(&h, &FlakyConfig::default())
            .into_iter()
            .map(|f| (f.flow, f.flakiness))
            .collect();
        assert_eq!(
            all,
            [
                ("a".to_owned(), Flakiness::NotFlaky),
                ("b".to_owned(), Flakiness::Confirmed),
                ("c".to_owned(), Flakiness::Suspected),
            ]
        );
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        /// flow, commit, condition, outcome, source, default branch, excluded, time.
        type Row = (u8, u8, u8, u8, u8, bool, bool, i64);

        fn any_run() -> impl Strategy<Value = Row> {
            (
                0..3u8,
                0..2u8,
                0..2u8,
                0..4u8,
                0..8u8,
                any::<bool>(),
                any::<bool>(),
                0..50i64,
            )
        }

        fn build(rows: &[Row]) -> Vec<HistoryRun> {
            rows.iter()
                .enumerate()
                .map(
                    |(
                        i,
                        &(flow, commit, cond_n, outcome, source, default_branch, excluded, at),
                    )| {
                        HistoryRun {
                            run: Uuid::from_u128(i as u128 + 1),
                            flow: format!("flow{flow}"),
                            commit: Some(format!("c{commit}")),
                            condition: cond(cond_n),
                            outcome: [
                                FlowOutcome::Passed,
                                FlowOutcome::Failed,
                                FlowOutcome::Interrupted,
                                FlowOutcome::Unknown,
                            ][usize::from(outcome)],
                            source: [
                                SourceKind::JunitFinal,
                                SourceKind::ArtifactFile,
                                SourceKind::LiveConsole,
                                SourceKind::DebugLog,
                                SourceKind::ProcessExit,
                                SourceKind::AgentObserved,
                                SourceKind::ServerInferred,
                                SourceKind::Declared,
                            ][usize::from(source)],
                            finality: Finality::Verified,
                            default_branch,
                            at_unix_ms: at,
                            exclusion: excluded.then_some(Exclusion::Infrastructure),
                        }
                    },
                )
                .collect()
        }

        proptest! {
            #[test]
            fn the_order_of_the_history_does_not_matter(
                rows in prop::collection::vec(any_run(), 0..40),
                seed in any::<u64>(),
            ) {
                let history = build(&rows);
                let mut shuffled = history.clone();
                // A deterministic shuffle: sort by a hash of the run and the seed.
                shuffled.sort_by_key(|r| (r.run.as_u128() ^ u128::from(seed)).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                prop_assert_eq!(
                    classify(&history, &FlakyConfig::default()),
                    classify(&shuffled, &FlakyConfig::default())
                );
            }

            #[test]
            fn a_run_of_another_flow_changes_no_verdict(
                rows in prop::collection::vec(any_run(), 0..40),
                extra in any_run(),
            ) {
                let history = build(&rows);
                let before = classify(&history, &FlakyConfig::default());
                let mut with = history;
                let mut unrelated = build(&[extra]).remove(0);
                unrelated.flow = "unrelated".into();
                unrelated.run = Uuid::from_u128(u128::MAX);
                with.push(unrelated);
                let after: Vec<_> = classify(&with, &FlakyConfig::default())
                    .into_iter()
                    .filter(|f| f.flow != "unrelated")
                    .collect();
                prop_assert_eq!(before, after);
            }

            #[test]
            fn without_a_counted_pass_nothing_is_confirmed(
                rows in prop::collection::vec(any_run(), 0..40),
            ) {
                // Passes read from anything but a parsed Maestro source are not passes.
                let history: Vec<_> = build(&rows)
                    .into_iter()
                    .map(|mut r| {
                        if r.outcome == FlowOutcome::Passed {
                            r.source = SourceKind::ServerInferred;
                        }
                        r
                    })
                    .collect();
                for v in classify(&history, &FlakyConfig::default()) {
                    prop_assert_ne!(v.flakiness, Flakiness::Confirmed);
                }
            }

            #[test]
            fn confirmed_evidence_names_only_counted_runs_of_that_flow(
                rows in prop::collection::vec(any_run(), 0..40),
            ) {
                let history = build(&rows);
                for v in classify(&history, &FlakyConfig::default()) {
                    for e in &v.evidence {
                        let ids: Vec<Uuid> = match e {
                            Evidence::PassedAndFailed { passed, failed, .. } => {
                                passed.iter().chain(failed).copied().collect()
                            }
                            Evidence::Flips { runs, .. } => runs.clone(),
                        };
                        for id in ids {
                            let r = history.iter().find(|r| r.run == id).unwrap();
                            prop_assert_eq!(&r.flow, &v.flow);
                            prop_assert!(matches!(r.outcome, FlowOutcome::Passed | FlowOutcome::Failed));
                            prop_assert!(r.outcome == FlowOutcome::Passed || r.exclusion.is_none());
                        }
                    }
                }
            }
        }
    }
}
