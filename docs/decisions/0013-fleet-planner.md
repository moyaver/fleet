# ADR-0013: Fleet Planner: static, duration-aware CI sharding

**Status:** Accepted (owner decision) · **Date:** 2026-11-04 · **Issues:** [#20](https://github.com/moyaver/fleet/issues/20) (agent), [#21](https://github.com/moyaver/fleet/issues/21) (server), [#22](https://github.com/moyaver/fleet/issues/22) (planner)

## Context
Maestro's `--shard-split` spreads flows across emulators on **one machine**. Spreading a suite across **several CI jobs or machines** is left to each team's own scripts, and a naive equal-count split leaves the pipeline waiting on the slowest job. Fleet already stores per-flow durations, so it can plan better than a count split. Spec v1.0 §17 rules out "CI orchestration or job scheduling"; static planning stays on the right side of that line because the CI still launches every job and Fleet only computes and records an assignment.

## Decision
- **A `fleet-planner` CLI** runs in the planning CI job, before the test jobs:
  1. discovers flows with Maestro's semantics: `config.yaml` `flows` globs, include/exclude tags; each `executionOrder` sequence is one indivisible, ordered unit;
  2. honours constraints: platform and device capabilities, and mutually exclusive groups (for example flows sharing a test account), declared explicitly because Maestro has no such concept;
  3. estimates each unit's duration from Fleet history (per-flow p50, with p90 as a tie-breaker), falling back to supplied estimates or an even split when there's no history;
  4. allocates **Longest Processing Time first** across N shards;
  5. writes an **immutable assignment manifest** and a GitHub Actions matrix, and publishes the manifest to the server.
- **The agent** runs a shard of a plan (`--plan <manifest> --shard <id>`), passes exactly those flows to Maestro (a command that already names flow paths is refused, exit 2), and reports the plan ID and planned flows before Maestro starts.
- **The server** stores plans, links agent runs to their shard, and shows planned totals before any job starts and **planned vs actual** per shard afterwards.
- **Static only.** A dynamic work queue (leases, idle runners pulling work) is orchestration and stays out of v1, unless measurements show static sharding falls short.
- Two emulators per runner (planner across machines, `--shard-split` within one) is supported but not recommended until benchmarked: research 08 showed two emulators already stress a 16 GiB host.

## Consequences
- New CLI binary (`crates/fleet-planner`), built in M5 when history exists; the agent hook lands in M2 and plan storage in M3.
- Plan quality improves as history accumulates; no ML.
- Flows run by a plan keep Maestro's semantics; the planner never reorders inside an `executionOrder` sequence.
