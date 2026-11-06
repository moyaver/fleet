// SPDX-License-Identifier: AGPL-3.0-only
//! The plan table on Screen B: each shard of a Fleet plan, planned against actual, and the
//! slowest shard against an equal-count split of the same flows (ADR-0013, issue #81). Pure: every
//! number comes from the server's plan view, so this module needs no browser.

use fleet_protocol::live::PlanView;
use fleet_ui::charts::format::format_duration;

/// One shard's row. Times are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardRow {
    /// The shard number, from 0.
    pub shard_id: i32,
    /// Flows the plan assigned to it.
    pub flows: usize,
    /// The planner's estimate.
    pub planned_ms: Option<i64>,
    /// First flow start to last flow end; `None` while one of its flows is still running.
    pub actual_ms: Option<i64>,
}

impl ShardRow {
    /// Actual minus planned, when both are known.
    #[must_use]
    pub fn difference_ms(&self) -> Option<i64> {
        Some(self.actual_ms? - self.planned_ms?)
    }
}

/// A plan's shards and the slowest-shard numbers the page states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanComparison {
    /// The plan's ID.
    pub plan_id: String,
    /// One row per shard, by shard number.
    pub shards: Vec<ShardRow>,
    /// The slowest shard by estimate.
    pub planned_max_ms: Option<i64>,
    /// The slowest shard by actual time; `None` until every shard's time is known.
    pub actual_max_ms: Option<i64>,
    /// The slowest shard had the flows been split by count alone, with the same estimates.
    pub equal_count_max_ms: Option<i64>,
}

impl PlanComparison {
    /// The comparison for one plan as the server describes it.
    #[must_use]
    pub fn new(plan: &PlanView) -> Self {
        let shards: Vec<ShardRow> = plan
            .shards
            .iter()
            .map(|s| ShardRow {
                shard_id: s.shard_id,
                flows: s.flows.len(),
                planned_ms: s.estimate_ms,
                actual_ms: s.actual_ms,
            })
            .collect();
        Self {
            plan_id: plan.plan_id.clone(),
            planned_max_ms: max_of(shards.iter().map(|s| s.planned_ms)),
            actual_max_ms: max_of(shards.iter().map(|s| s.actual_ms)),
            equal_count_max_ms: plan.equal_count_estimate_ms,
            shards,
        }
    }

    /// The share of the equal-count slowest shard the plan saves, by estimate, rounded down.
    /// Negative when the plan is worse than a count split. `None` without a baseline.
    #[must_use]
    pub fn saved_percent(&self) -> Option<i64> {
        let (planned, equal) = (self.planned_max_ms?, self.equal_count_max_ms?);
        (equal > 0).then(|| (equal - planned) * 100 / equal)
    }

    /// The sentences shown under the table, in order: the slowest shard's actual against its
    /// planned time, then the saving over an equal-count split.
    #[must_use]
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let (Some(planned), Some(actual)) = (self.planned_max_ms, self.actual_max_ms) {
            lines.push(format!(
                "Slowest shard: {} planned, {} actual ({}).",
                format_duration(planned),
                format_duration(actual),
                signed(actual - planned)
            ));
        } else if self.planned_max_ms.is_some() {
            lines.push("Actual times show once every shard's flows have ended.".to_owned());
        }
        if let (Some(equal), Some(saved)) = (self.equal_count_max_ms, self.saved_percent()) {
            let split = format_duration(equal);
            lines.push(if saved > 0 {
                format!(
                    "An equal-count split would have made the slowest shard {split}: the planner saved {saved}% of it, by estimate."
                )
            } else {
                format!(
                    "An equal-count split would have made the slowest shard {split}: by estimate, the planner did not beat it."
                )
            });
        }
        lines
    }
}

/// The largest value, or `None` if any shard lacks one.
fn max_of(values: impl Iterator<Item = Option<i64>>) -> Option<i64> {
    values.collect::<Option<Vec<i64>>>()?.into_iter().max()
}

/// A duration for a table cell, or a dash when it is not known.
#[must_use]
pub fn show(ms: Option<i64>) -> String {
    ms.map_or_else(|| "—".to_owned(), format_duration)
}

/// A signed duration, `+` for later than planned and `-` for earlier.
#[must_use]
pub fn signed(ms: i64) -> String {
    let sign = if ms < 0 { "-" } else { "+" };
    format!("{sign}{}", format_duration(ms.abs()))
}

#[cfg(test)]
mod tests {
    use fleet_protocol::live::PlanShardView;
    use uuid::Uuid;

    use super::*;

    fn shard(id: i32, flows: usize, planned: i64, actual: Option<i64>) -> PlanShardView {
        PlanShardView {
            shard_id: id,
            flows: (0..flows).map(|i| format!("flow-{i}.yaml")).collect(),
            estimate_ms: Some(planned),
            agent_runs: vec![Uuid::nil()],
            executed: 0,
            actual_ms: actual,
        }
    }

    fn plan(equal: Option<i64>, shards: Vec<PlanShardView>) -> PlanView {
        PlanView {
            plan_id: "plan-1".into(),
            planned_total: i32::try_from(shards.iter().map(|s| s.flows.len()).sum::<usize>())
                .unwrap(),
            equal_count_estimate_ms: equal,
            shards,
        }
    }

    #[test]
    fn a_plan_gives_each_shards_difference_and_the_slowest_shards_saving() {
        let view = PlanComparison::new(&plan(
            Some(400_000),
            vec![
                shard(0, 3, 300_000, Some(330_000)),
                shard(1, 2, 200_000, Some(190_000)),
            ],
        ));
        assert_eq!(view.planned_max_ms, Some(300_000));
        assert_eq!(view.actual_max_ms, Some(330_000));
        assert_eq!(view.shards[0].difference_ms(), Some(30_000));
        assert_eq!(view.shards[1].difference_ms(), Some(-10_000));
        assert_eq!(view.saved_percent(), Some(25), "(400 - 300) / 400");
        assert_eq!(
            view.summary(),
            vec![
                format!(
                    "Slowest shard: {} planned, {} actual (+30 s).",
                    format_duration(300_000),
                    format_duration(330_000)
                ),
                format!(
                    "An equal-count split would have made the slowest shard {}: the planner saved 25% of it, by estimate.",
                    format_duration(400_000)
                ),
            ]
        );
    }

    #[test]
    fn actual_times_wait_until_every_shard_has_ended() {
        let view = PlanComparison::new(&plan(
            None,
            vec![shard(0, 1, 60_000, Some(50_000)), shard(1, 1, 60_000, None)],
        ));
        assert_eq!(view.actual_max_ms, None, "a shard is still running");
        assert_eq!(view.shards[1].difference_ms(), None);
        assert_eq!(
            view.summary(),
            vec!["Actual times show once every shard's flows have ended.".to_owned()],
            "no baseline, so no saving is claimed"
        );
        assert_eq!(view.saved_percent(), None);
    }

    #[test]
    fn a_plan_worse_than_a_count_split_says_so_and_never_claims_a_saving() {
        let view = PlanComparison::new(&plan(
            Some(400_000),
            vec![shard(0, 1, 500_000, None), shard(1, 1, 100_000, None)],
        ));
        assert_eq!(view.saved_percent(), Some(-25));
        assert!(
            view.summary()
                .last()
                .is_some_and(|line| line.ends_with("the planner did not beat it.")),
            "{:?}",
            view.summary()
        );
    }

    #[test]
    fn cells_show_a_dash_for_unknown_times_and_signs_differences() {
        assert_eq!(show(None), "—");
        assert_eq!(show(Some(1_500)), format_duration(1_500));
        assert_eq!(signed(-2_000), format!("-{}", format_duration(2_000)));
        assert_eq!(signed(0), format!("+{}", format_duration(0)));
    }
}
