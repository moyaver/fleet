# ADR-0003: Live progress from `maestro.log`, bundle folders and stdout

**Status:** Accepted (Phase 0 passed, 2026-11-04) · **Date:** 2026-11-03 · **Research:** [01 §13–15](../../research/01-maestro-cli-internals.md), [08](../../research/08-phase0-results.md)

## Context
With `--format junit` (the suite path), Maestro 2.12.1 prints only one line per *completed* flow to stdout: no flow starts, no steps. Planned flows, shard↔device mapping, flow starts and every step's RUNNING/COMPLETED/FAILED go to the session `maestro.log`. Per-flow bundle folders appear at flow start, failure screenshots when a step fails, and `commands.json`/`manifest.json` at flow end. JUnit is written once, after all shards finish, and not at all on SIGINT/SIGTERM.

## Decision
- **Three evidence lanes**, merged as provenance claims:
  1. a `maestro.log` tailer (primary for plan, starts, steps, device and shard);
  2. a stdout parser (flow completion);
  3. a filesystem watcher (bundles, `manifest.json`, `commands.json`).
- JUnit reconciles at the end.
- **The agent sets the invocation:**
  - `MAESTRO_CLI_LOG_PATTERN_FILE` for a machine-parseable log line;
  - analytics and update check disabled;
  - `--flatten-debug-output --debug-output <fresh dir>`;
  - pinned `--device`.
- **Version-profiled parsers** (≥2.7 bundle layout; <2.7 best-effort), with a "degraded" mode that never makes up pass/fail.
- **Propose an upstream `--events` NDJSON reporter** built on `OrchestraListener`. Switch to it when it is available.

## Consequences
- If Phase 0 shows `maestro.log` is not flushed per line, live step progress falls back to per-flow granularity. The UI must handle both.
- Skipped, interrupted and unknown are always inferred, and labelled as such.

## Phase 0 outcome (2026-11-04)
[Research 08](../../research/08-phase0-results.md), on Maestro 2.12.1 with captured fixtures: `maestro.log` **is** flushed per line (every line seen within one 50 ms poll), and step `RUNNING`/`COMPLETED`/`FAILED`/`WARNED`/`SKIPPED` lines are in it at the default level. The strategy stands, with these refinements:
- attribute lines by the `[shard N]` message prefix, never by thread name or per-flow log file;
- join non-`MFLT1|` lines (stack traces) to the previous log record;
- treat an empty or `RUNNING`-containing `commands.json` as "aborted", never as a result;
- take the plan from `[shard N] Selected device … with execution plan …` (present for single files too);
- take the device serial from that line; JUnit `device=` is only the AVD name.

