# 08: Phase 0 results — Maestro on a real emulator

**Date:** 2026-11-04 · **Milestone:** M1 · **Answers:** [research 01 §15](01-maestro-cli-internals.md#15-what-phase-0-must-verify-against-a-real-emulator) · **Fixtures:** [`fixtures/maestro/2.12.1/`](../fixtures/maestro/2.12.1/) · **Decision:** [ADR-0003](../docs/decisions/0003-live-signal-strategy.md) (now Accepted)

**Bottom line:** the live-signal strategy works. `maestro.log` is flushed line by line: a tailer polling every 50 ms saw 100% of session-log lines within one poll (p95 ≈ 50 ms, max 55 ms across 8 scenarios). Per-step `RUNNING`/`COMPLETED`/`FAILED`/`WARNED`/`SKIPPED` lines are in it at the default level, prefixed with `[shard N]`. The machine log pattern works. Research 01 held almost everywhere; the corrections are in §3 and they change parser design, not the strategy.

## 1. Setup

| | |
|---|---|
| Maestro | **2.12.1** (latest release, published 2026-11-02), run from the release zip, sha256 verified |
| Host | macOS 26 on Apple M1 Pro, 16 GiB; OpenJDK 26.0.1 |
| Devices | two headless, read-only instances of one AVD (`Pixel_10a_s3`, Android API 37, Google Play arm64 image): `emulator-5600` and `emulator-5602` |
| App | the built-in Settings app, so fixtures need no APK and contain nothing private ([flows](../examples/flows/phase0/README.md)) |
| Capture | `cargo xtask fixtures capture`: byte-exact stdout/stderr, per-line arrival times, a 50 ms filesystem poll, every `maestro.log` line as the tailer saw it, `ps` snapshots every 2 s, exit status. Imported with `cargo xtask fixtures import` (paths normalized to `<REPO>`, `<CAPTURE>`, `<MAESTRO_HOME>`, `<HOME>`, `<USER>`; logcat cut to 32 KiB). See [`fixtures/maestro/README.md`](../fixtures/maestro/README.md). |
| Agent-like env | `MAESTRO_CLI_NO_ANALYTICS=1`, `MAESTRO_DISABLE_UPDATE_CHECK=true`, `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true`, `XDG_STATE_HOME=<CAPTURE>/state` |
| Invocation (canonical) | `maestro --device <serial> test --format junit --output <run>/report.xml --flatten-debug-output --debug-output <run>/debug <flows>` |

24 scenarios were captured; every one is a fixture directory. A second evidence base was analysed read-only: ~550 sweeps and 532 local runs of a private app on **Maestro 2.11.0** (physical phone, network-adb devices and emulators). Its findings corroborate the ones below; no data from it is in the repository.

## 2. The 17 questions

| Q | Status | Answer | Fixtures |
|---|---|---|---|
| 1 Console | **Answered** | Suite path: no flow-start lines. `[Passed] <name> (<dur>)` / `[Failed] <name> (<dur>) (<msg>)`, then a blank line, `N/M Flow(s) Failed` or `N/M Flows Passed in <dur>`, then a Unicode-box **Maestro Cloud promotion** (≈1 KB). Completion order is execution order, not file order (`bravo, charlie, delta-fails, alpha`). Sharded: every line prefixed `[shard N] `, one summary per shard, a `Will split 4 flows across 2 shards …` / `Will run 2 shards, with all 4 flows in each shard` header. Durations `Ns`, `Nm Ns` (2.11 data adds `Nms`, `Nm`, `Nh Nm Ns`). No ANSI under a pipe. Device loss prints `[Failed] slow (32s)` **with no message**. | `suite`, `shard-split-2`, `shard-all-2`, `device-killed` |
| 2 Log liveness | **Answered: live** | Every session-log line reached the tailer within one 50 ms poll (p50 25–34 ms, p95 43–53 ms, max 55 ms; lag = observed time − the line's own `UNIX_MILLIS`). Steps appear as `<description> RUNNING` then `COMPLETED`/`FAILED`/`WARNED`/`SKIPPED` from `maestro.cli.runner.CliConsoleListener`; ≈3–5 lines per step at INFO. | `pass-machine-log`, `features`, `suite` |
| 3 Log pattern | **Answered: works, with a catch** | `MAESTRO_CLI_LOG_PATTERN_FILE='MFLT1\|%d{UNIX_MILLIS}\|%t\|%level\|%logger\|%enc{%msg}{CRLF}%n'` is honoured in the session log. **`%t` is not a shard identity**: both shards use `DefaultDispatcher-worker-N` threads interchangeably; use the `[shard N]` message prefix. **Exception stack traces bypass the pattern**: log4j appends them raw after the line (49 of 136 lines in `device-killed`), so lines not starting `MFLT1\|` continue the previous record. The default pattern (`HH:mm:ss.SSS [LEVEL] logger.method: msg`) has no date. | `pass-machine-log`, `shard-split-2`, `device-killed`, `pass` |
| 4 `--verbose` | **Answered** | Stdout grows ~45× (3.5 KB vs 79 B for one flow) and **emits ANSI colour even under a pipe** (`[TRACE]`, `[ INFO]`), including the full analytics JSON. Not useful to the agent; don't pass it. | `verbose` |
| 5 Output paths | **Answered (5 of 6 combos)** | Exactly as research 01 §4.1 says, docs are wrong: none → `$STATE/tests/<ts>/`; `--test-output-dir T` → log in `$STATE/tests/<ts>/`, bundles in `T/<ts>/`; `--debug-output D` → `D/.maestro/tests/<ts>/`; **`D = T` splits into `X/<ts>/` and `X/.maestro/tests/<ts>/`**; flatten + D → `D/`. Flatten alone writes to `~` and was deliberately not run. | `paths-*`, all canonical runs |
| 6 Write timing | **Answered** | Per flow: flow dir (+ `logs/maestro.log`, ~0.2 s later) at start → on failure `screen-hierarchy/step-NNN-….json`, then `screenshots/step-NNN-….png.tmp` renamed to `.png` → `commands.json` (**only at flow end**) → `logs/device-logcat.txt` → stdout `[Passed]/[Failed]` → `manifest.json`, in the same 50 ms poll as the next flow's directory → `report.xml` ≈50 ms after the last manifest. | `suite`, `fail`, `shard-split-2` |
| 7 `commands.json` | **Answered** | Array of `{command, metadata}`; metadata `{sequenceNumber, depth, timestamp, duration, status, evaluatedCommand}`. `sequenceNumber`/`depth` 0 are emitted. Statuses seen: `COMPLETED`, `FAILED`, `WARNED` (optional assert), `SKIPPED` (`runFlow when:` false), and **`RUNNING`** (left behind on device loss). Nested `runFlow`/`retry`/`repeat` children have `depth: 1`. Retry attempts show as repeated child entries plus `Attempt N failed for retry command` in the log. **Planted `-e` and `MAESTRO_*` values appear in clear** in `commands.json` (`evaluatedCommand`, `defineVariables`), in both `maestro.log`s and in argv. On SIGTERM `commands.json` is **written empty (0 bytes)**. | `features`, `retry-fail`, `device-killed`, `sigterm` |
| 8 Per-flow log contamination | **Answered: yes** | Each per-flow `logs/maestro.log` ends with the **next** flow's `Running flow <name>` line, **even across shards** (`bravo-shard-1` contains `Running flow charlie`, which ran on shard 2). Never attribute per-flow log lines by file; use the session log. | `shard-split-2`, `shard-all-2` |
| 9 JUnit | **Answered** | One `<testsuite name="Test Suite" device="<AVD name>">` per shard **in shard order**; `<testcase id name classname file time timestamp status="SUCCESS\|ERROR">`; failure text keeps the `[shard N] ` prefix; device loss puts the exception message in `<failure>`. Timestamps are local time **without a zone**. **Two emulators of one AVD both report `device="Pixel_10a_s3"`**: JUnit can't tell them apart. With `--shard-all`, the same testcase appears once per shard. | `shard-split-2`, `shard-all-2`, `device-killed` |
| 10 Device correlation | **Answered (pinned)** | Session log: `[shard N] Selected device <serial> using port <P> with execution plan ExecutionPlan(flowsToRun=[<paths>], …)` gives shard → serial → planned flows, for single files too. `Created execution plan:` appears only for directory input. Flow dirs carry `-shard-N`. Rule: **shard N ⇔ JUnit suite N ⇔ `*-shard-N` dirs ⇔ the serial on that shard's `Selected device` line.** With `--device "A,B"`, shard 1 got A and shard 2 got B. Unpinned selection was not run (it would have used a device in use by others). | `shard-split-2`, `shard-all-2` |
| 11 Exit codes | **Answered** (driver-install failure not reproduced) | pass 0 · test failure 1 · no such device 1 (`Device emulator-5999 was requested, but it is not connected.` on stderr) · bad flag **2** (picocli `Unknown option`) · device lost mid-run 1 · SIGINT **130** after 0.6 s · SIGTERM **143** after 0.45 s · SIGKILL by signal 9. On INT/TERM: **no `report.xml`, no `manifest.json`**, `commands.json` present (empty on TERM); on KILL only logs. Shutdown is fast (<1 s), not the 10–15 s research 01 feared. | `pass`, `fail`, `no-device`, `bad-flag`, `device-killed`, `sig*` |
| 12 Emulator crash | **Answered** | `maestro.android.DeviceServerDiedException: Device server died during 'viewHierarchy' on emulator-5602 (…ms since last byte, connection age …ms): StatusRuntimeException: UNAVAILABLE`, caused by `IOException: Command failed (host:transport:emulator-5602): device offline`. The flow fails; stdout shows no reason; `commands.json` keeps `RUNNING` entries; JUnit has the message. (2.11 data: every later flow also fails.) | `device-killed` |
| 13 Concurrent adb | **Answered (no slowdown)** | A suite with an adb poller (`dumpsys activity`, `getprop`, `dumpsys meminfo` every 5 s) took 54.3 s vs 57.2 s alone, same results. `uiautomator dump` was deliberately not tested. | `suite-with-adb-load` |
| 14 Process tree | **Answered** | The `maestro` launcher `exec`s Java: the PID CI waits on is the JVM. JVM RSS 60–250 MB, CPU bursts to ~200%. On macOS it also spawns `xcrun simctl list -j` even for Android runs. | `suite` (`events.jsonl` `proc`) |
| 15 Offline runner | **Not tested** | Needs network isolation of the host. The agent's env already disables analytics and the update check; verify in the nightly job. | — |
| 16 `executionOrder` | **Answered** | With `continueOnFailure: false` and a failing first flow: stdout `Flow first-fails failed and continueOnFailure is set to false, aborting running sequential Flows`; the skipped flow is **absent** from stdout, JUnit and the summary (`1/1 Flow Failed`). Also prints `Requested 1 shards, but it cannot be higher than the number of flows (0)`. | `execution-order` |
| 17 Versions | **Partly** | 2.12.1 is the latest release; 2.11.0 (N−1) agrees on every format above except as noted in §3. Parsers must be profiled by version and survive N−1..N+1 (nightly matrix). | all |

## 3. Corrections to research 01

1. **Thread is not shard identity** (Q3). Use `[shard N] `.
2. **Stack traces escape the log pattern** (Q3). The log parser joins continuation lines.
3. **Per-flow logs leak across flows and shards** (Q8).
4. **`commands.json` is not final on abnormal ends**: `RUNNING` entries on device loss, a 0-byte file on SIGTERM, truncated JSON seen in 2.11 data. Treat as evidence of "aborted", never as a result.
5. **The logcat file is `logs/device-logcat.txt`**, and failure screenshots appear first as `.png.tmp`.
6. **`Created execution plan` is directory-input only**; the per-shard `Selected device … with execution plan` line always carries the plan.
7. **`--verbose` writes ANSI to a pipe.**
8. **Shutdown on SIGINT/SIGTERM takes under a second**, so GitHub's 7.5 s window is enough for Maestro itself; the agent's own finalization is still the tight part.
9. **JVM warnings on stderr** under Java 24+ (`WARNING: Final field … mutated reflectively`, `sun.misc.Unsafe`) are noise the stderr parser must ignore.

## 4. Progress states, classified with evidence

| State | Class | Source | Evidence |
|---|---|---|---|
| Planned flows | **Observable live** | `[shard N] Selected device … ExecutionPlan(flowsToRun=[…])` (seconds after start) | `shard-split-2`, `pass-machine-log` |
| Device ↔ shard | **Observable live** | same line: serial + port | `shard-split-2` |
| Flow started | **Observable live** | `TestSuiteInteractor` `[shard N]  Running flow <name>`; flow dir | `suite` |
| Step running / completed / failed / warned / skipped | **Observable live** | `CliConsoleListener` `<desc> RUNNING\|COMPLETED\|FAILED\|WARNED\|SKIPPED` | `features`, `retry-fail` |
| Step failure evidence | **Observable live** | `screen-hierarchy/step-NNN-*.json`, `screenshots/step-NNN-*.png` | `fail` |
| Retry attempt (command) | **Observable live** | `Attempt N failed for retry command` | `retry-fail` |
| Flow completed + outcome (provisional) | **Observable live** | stdout `[Passed]/[Failed]`; `manifest.json` | `suite` |
| Exact step timing | **At flow end** | `commands.json` | `features` |
| Final outcome (authoritative) | **At run end** | JUnit; exit code | `suite`, `shard-split-2` |
| Skipped flow (`executionOrder`) | **Inferred** | planned − run, plus the abort line | `execution-order` |
| Interrupted | **Inferred** | exit 130/143/signal + flow dir without `manifest.json` + no JUnit | `sigint`, `sigterm`, `sigkill` |
| Aborted by device loss | **Observable** (cause) / **inferred** (which steps) | `DeviceServerDiedException` in the log and JUnit; `RUNNING` left in `commands.json` | `device-killed` |
| Flow-level retry | **Unavailable** | Maestro never retries flows locally | — |
| Which of two same-AVD emulators (from JUnit alone) | **Unavailable** | JUnit `device=` is the AVD name; use the log's serial | `shard-split-2` |

**M1 gate:** every progress state is classified with fixture evidence, and the device-correlation rule is confirmed (Q10). **Passed.**

## 5. Build vs buy (ADR-0009)

- **Allure 3.22.1** (`npx allure generate` on the `suite`, `shard-split-2` and `device-killed` JUnit files) reported **5 of 5 tests passed** with 4 "retries". It marked `delta-fails` (assertion failure) and `slow` (`DeviceServerDiedException`) as **passed**, and folded shard duplicates into retries. Maestro's `status="ERROR"` + `<failure>` is not read as a failure. That's a false green, which invariant I3 forbids, on top of having no live view, devices or steps. Doesn't change ADR-0009.
- **ReportPortal** was not run: it needs a ~10-container Docker deployment and the host's Docker daemon was off. Research 07's desk review stands: JUnit import is end-of-run only, and live data needs a per-framework agent that doesn't exist for Maestro.

## 6. Upstream `--events` reporter (open decision O3)

Not posted: opening a discussion on `mobile-dev-inc/maestro` is the owner's call. Draft:

> **Proposal: a machine-readable live event stream (`--events <file|fd>`)**
> Tools that wrap `maestro test` in CI today parse `maestro.log` (`CliConsoleListener` lines) and watch bundle folders to show live progress. That works (lines are flushed promptly) but is fragile: thread names aren't shard identities, stack traces escape the log pattern, per-flow logs pick up the next flow's first line, and `commands.json` is only written at flow end. An NDJSON stream built on `OrchestraListener` — `{"type":"flow.started","shard":1,"flow":"login","device":"emulator-5554",…}`, `step.started/completed` with `sequenceNumber`/`depth`, `flow.completed` with status and duration — would give every tool a stable contract. Happy to contribute the implementation.

## 7. What M2 takes from this

- **Lane A (session log) is primary and live**; parse `MFLT1|…` with continuation lines, attribute by `[shard N]`, never by thread or per-flow log file.
- **Lane B (stdout)**: `[shard N] `-prefixed `[Passed]/[Failed]` and summaries; ignore the promotion box and JVM warnings.
- **Lane C (files)**: flow dir = started; `screenshots/*.png` (not `.tmp`) and `screen-hierarchy/*.json` = step failed; `manifest.json` = flow done; `commands.json` = exact timings, unless empty/`RUNNING`.
- **Lane E (JUnit)**: positional per shard; `device=` only as the AVD name.
- **Redaction (I6)** must cover `commands.json`, both logs, and argv: planted values appear in all of them.
- **Exit classification**: 0 pass · 1 + JUnit = test failures · 1 without JUnit = infra/usage (`no-device`) · 2 = usage · 130/143/signal = interrupted.
