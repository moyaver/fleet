// SPDX-License-Identifier: AGPL-3.0-only
//! Fleet Planner manifests (ADR-0013): an immutable assignment of flows to CI shards.
//!
//! The planner writes one, the agent runs one shard of it (`--plan <manifest> --shard <id>`),
//! and the server stores it (`POST /api/v1/plans`) so planned totals are known before any job
//! starts. JSON keys are camelCase.

use serde::{Deserialize, Serialize};

/// A plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PlanManifest {
    /// Stable identifier, unique per project.
    pub plan_id: String,
    /// Total flows across shards, as the planner counted them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_flows: Option<u32>,
    /// The workspace `config.yaml`, relative to the repository root. The agent passes it to
    /// Maestro with `--config`, so `executionOrder` and tag rules still apply to the shard's flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// SHA-256 of `config_path` at planning time; the agent refuses to run if it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
    /// The shards.
    pub shards: Vec<PlanShard>,
}

/// One shard of a plan: the flows one CI job runs, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PlanShard {
    /// Shard number, from 0.
    pub id: u32,
    /// The planner's duration estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_seconds: Option<u64>,
    /// Flow files, in order.
    pub flows: Vec<String>,
}

impl PlanManifest {
    /// Flows planned across all shards.
    #[must_use]
    pub fn planned_total(&self) -> usize {
        self.shards.iter().map(|s| s.flows.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_manifest_format_parses() {
        let m: PlanManifest = serde_json::from_str(
            r#"{"planId":"p1","totalFlows":2,"shards":[{"id":0,"estimatedSeconds":5,"flows":["a.yaml"]},{"id":1,"flows":["b.yaml"]}]}"#,
        )
        .unwrap();
        assert_eq!((m.plan_id.as_str(), m.planned_total()), ("p1", 2));
        assert_eq!(m.shards[0].estimated_seconds, Some(5));
    }
}
