// SPDX-License-Identifier: AGPL-3.0-only
//! Wire-form builders shared by the pipeline view-model tests.

use fleet_protocol::live::{
    AgentRunView, AttemptView, FlowView, JobView, LiveFrame, Snapshot, SuiteView,
};
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

pub(super) const ATTEMPT: u128 = 1;
pub(super) const JOB: u128 = 10;
pub(super) const RUN: u128 = 20;
pub(super) const SUITE: u128 = 30;

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

pub(super) fn attempt(version: i64, planned_total: Option<i64>) -> AttemptView {
    parse(json!({
        "id": id(ATTEMPT), "project_id": id(100), "provider": "github_actions",
        "run_key": "acme/app:9001", "repository": "acme/app",
        "commit_sha": "0123456789abcdef", "git_ref": "refs/heads/main", "attempt": 2,
        "version": version, "planned_total": planned_total, "jobs": 1, "open_jobs": 1,
        "first_seen_at": "2026-01-01T00:00:00Z"
    }))
}

pub(super) fn job(
    n: u128,
    key: &str,
    lifecycle: &str,
    outcome: Option<&str>,
    source: Option<&str>,
) -> JobView {
    parse(json!({
        "id": id(n), "pipeline_attempt_id": id(ATTEMPT), "job_key": key, "name": null,
        "lifecycle": lifecycle, "outcome": outcome, "outcome_source": source,
        "reopened_count": 0, "rev": 1
    }))
}

pub(super) fn run(n: u128, job: u128, liveness: &str) -> AgentRunView {
    parse(json!({
        "id": id(n), "job_id": id(job), "worker_id": null, "runner_name": "runner-1",
        "state": "active", "trust_level": "trusted", "committed_seq": 0, "gaps": 0,
        "last_frame_at": "2026-01-01T00:00:00Z", "late_resume": false, "plan_id": null,
        "plan_shard_id": null, "maestro_exit_code": null, "rev": 1, "liveness": liveness
    }))
}

pub(super) fn suite(n: u128, job: u128, planned: Option<i32>) -> SuiteView {
    parse(json!({
        "id": id(n), "job_id": id(job), "agent_run_id": id(RUN), "shard_mode": "split",
        "shard_count": 2, "planned_count": planned, "plan_source": null,
        "lifecycle": "running", "outcome": null, "outcome_source": null,
        "report_status": null, "exit_code": null, "rev": 1
    }))
}

pub(super) fn flow(n: u128, suite: u128, name: &str, state: &str, finality: &str) -> FlowView {
    parse(json!({
        "id": id(n), "job_id": id(JOB), "suite_execution_id": id(suite),
        "agent_run_id": id(RUN), "device_session_id": null, "shard_number": 1, "ordinal": n,
        "name": name, "path": null, "state": state, "outcome_source": null,
        "finality": finality, "has_conflict": false, "failure_message": null,
        "duration_ms": null, "steps": 0, "rev": 1
    }))
}

pub(super) fn snapshot(
    version: i64,
    jobs: Vec<JobView>,
    runs: Vec<AgentRunView>,
    suites: Vec<SuiteView>,
    flows: Vec<FlowView>,
) -> LiveFrame {
    LiveFrame::Snapshot {
        snapshot: Box::new(Snapshot {
            attempt: attempt(version, None),
            jobs,
            agent_runs: runs,
            suites,
            devices: vec![],
            flows,
            plans: vec![],
        }),
    }
}
