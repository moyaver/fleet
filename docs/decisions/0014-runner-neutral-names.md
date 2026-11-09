# ADR-0014: Runner-neutral names on the wire and in the schema

**Status:** Accepted (owner decision) · **Date:** 2026-11-09 · **Issues:** [#153](https://github.com/moyaver/fleet/issues/153) (ADR-0011 follow-up)

## Context
[ADR-0011](0011-name-and-runner-scope.md) keeps `fleet-protocol`, `fleet-core`, `fleet-store`, `fleet-server`, `fleet-ui` and `fleet-web` free of runner-specific types and names. A second runner (an instrumentation runner, Appium, XCUITest) is a new adapter beside `fleet-maestro`. The core still carried Maestro's names: the event `maestro.detected`, the artifact class `maestro_log`, the agent-run and telemetry columns, the `maestro_*` readings, the failure-fingerprint dimension `maestro`, and the Maestro redaction rules.

The project is pre-release. The owner decided that #153 is a clean start, as if the Maestro names had never existed, rather than a deprecation window.

## Decision
- **Names.** The wire, the schema and the configuration use runner-neutral names only:
  - event `runner.detected { runner, version, profile_supported }`, where `runner` is `maestro` for Maestro;
  - artifact class `runner_log`; `RunStarted`'s evidence flag `runner_log`; the exit class `runner_error`;
  - the process readings `runner_cpu_pct` and `runner_rss_bytes` (telemetry samples, their `unavailable` reasons, the metric series of the same ids and scope `runner`);
  - the agent-run fields `runner_exit_code`, `runner_exit_signal`, `runner_version` and `runner_profile_supported`, and the rollups `runner_cpu_avg` and `runner_rss_max`;
  - the failure-fingerprint dimension `runner` (its value is the runner's version, `major.minor`);
  - the configuration key `artifacts.max_bytes.runner_log`.
  The runner's identity is data: `runner` is `"maestro"` for Maestro, and a second runner sends its own value.
- **No compatibility.** There are no serde aliases, no legacy event kinds and no legacy class or metric names. Agents and server speak the neutral names only, and a server reads only that.
- **Protocol version.** `PROTOCOL_VERSION` stays `1`. Nothing is shipped yet, so the version does not mark a break.
- **Schema.** The migrations are edited in place: the columns of `agent_runs`, `host_samples` and the rollup tables, and the artifact class in `artifacts` and `project_artifact_classes`, carry the neutral names from `0002`, `0005`, `0008`, `0013`, `0015`, `0016` and `0032`. There is no rename migration. Before v1.0.0 migrations may be edited, because no database outside development holds them.
- **Clean break before v1.0.0.** An alpha install (`v0.1.0-alpha.1` to `alpha.3`) cannot be upgraded. Its database must be replaced with a fresh one, and alpha agents cannot talk to this server, so agents and server are upgraded together. The operator guide's upgrade section and the CHANGELOG (under *Breaking*) say so.
- **Upgrade rehearsal.** `cargo xtask upgrade-rehearsal` replays only release tags from `v1.0.0` on. The alpha tags cannot upgrade, so with no such tag it reports that there is nothing to rehearse and exits successfully. The CI job runs it unchanged.
- **Kept on purpose.**
  - Maestro's own output text stays where Fleet reads it: the exception class `maestro.android.DeviceServerDiedException` that marks a lost device in a failure message, the logger names in its log, and its session file `maestro.log`. These are data the runner writes, and the lint allows them (`RUNNER_OUTPUT`).
  - `ExitClass::RunnerError` stays a wire value of the protocol. Maestro's classification of that exit lives in `fleet-maestro`.
  - `retention_classes` is unchanged, because it names no artifact class.
- **Lint.** `cargo xtask lint` fails on a runner name (a `maestro_` identifier or string, a `Maestro` followed by a capital letter, a `MAESTRO_` variable, or a `"maestro.` wire string) in the runner-neutral crates' sources and tests and in every migration. The only exemption is the runner-output allowlist above.
- **Redaction.** The generic engine stays in `fleet-core` (`Redactor`, the token shapes, the argv walker that takes the flags a runner names). The Maestro rules (`-e`/`--env` values, the `MAESTRO_*` variables that carry flow data, and the CLI settings that are not secret) live in `fleet-maestro::redact`. `fleet-core` never names a runner's flags or variables.

## Consequences
- Every reader of the database, the JSON API or the configuration sees only the neutral names.
- A second runner writes the same columns with its own `runner` value and needs no core change for its names.
- The migration history is not an upgrade path before v1.0.0. From v1.0.0 on, migrations are never edited, and a change is a new file, as the operator guide says.
