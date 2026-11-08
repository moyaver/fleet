// SPDX-License-Identifier: AGPL-3.0-only
//! The failed flows of a pipeline attempt, by run, for its rerun manifest (issue #82).

use fleet_core::reproduce::FailedFlow;
use uuid::Uuid;

use crate::{Result, Store};

impl Store {
    /// The failed flows of an attempt, grouped by the agent run that ran them, in run order (the
    /// order the runs started, then each run's shard and ordinal). Only runs with a failed flow
    /// appear. `None` when the attempt is not in this project.
    pub async fn failed_flows(
        &self,
        project_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<Option<Vec<Vec<FailedFlow>>>> {
        let known = sqlx::query_scalar!(
            "SELECT id FROM pipeline_attempts WHERE id = $1 AND project_id = $2",
            attempt_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if known.is_none() {
            return Ok(None);
        }
        let rows = sqlx::query!(
            "SELECT f.agent_run_id, f.flow_path, f.flow_name
               FROM flow_executions f
               JOIN agent_runs r ON r.id = f.agent_run_id
              WHERE f.project_id = $1 AND f.pipeline_attempt_id = $2 AND f.state = 'failed'
              ORDER BY r.started_at, r.id, f.shard_number NULLS LAST, f.ordinal NULLS LAST,
                       f.started_at NULLS LAST, f.id",
            project_id,
            attempt_id
        )
        .fetch_all(self.pool())
        .await?;
        let mut groups: Vec<(Uuid, Vec<FailedFlow>)> = Vec::new();
        for row in rows {
            let flow = FailedFlow {
                path: row.flow_path,
                name: row.flow_name,
            };
            match groups.last_mut() {
                Some((run, flows)) if *run == row.agent_run_id => flows.push(flow),
                _ => groups.push((row.agent_run_id, vec![flow])),
            }
        }
        Ok(Some(groups.into_iter().map(|(_, flows)| flows).collect()))
    }
}
