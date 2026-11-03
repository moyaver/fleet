# 01 — Maestro CLI internals: which live progress signals a wrapper agent can observe

**Research date:** 2026-11-03
**Subject:** Maestro CLI **v2.12.1** (tag `v2.12.1`, commit `67d8a677`, released 2026-11-02). Source was read from a shallow clone of `github.com/mobile-dev-inc/maestro`, later deepened (blobless) to read history.
**Scope:** spec §3 (execution hierarchy), §4 "Progress collection" / "Accurate progress accounting", §13 (failure and interruption behaviour) and §14 Phase 0, all checked against how Maestro actually behaves.
**Method:** read-only. Nothing was built, installed or executed. Console output below comes from string templates in the source and from Maestro's own unit tests, never from a captured run. Anything that needs a real run is listed under §15 (Phase 0).

Evidence labels (the same convention as the sibling reports):

| Label | Meaning |
|---|---|
| **[V]** | Verified in Maestro source at tag `v2.12.1` (file path and line given), or in its unit tests. |
| **[D]** | Stated in the official docs (docs.maestro.dev) or the CHANGELOG. |
| **[I]** | Inferred from the source plus general JVM, log4j or Android behaviour. Plausible, but not proven. Phase 0 must confirm. |
| **[K]** | Prior knowledge that was not re-checked in this session. |

All source paths are relative to the repo root at `v2.12.1`. Most CLI files live under `maestro-cli/src/main/java/maestro/cli/`, abbreviated `cli/` below.

---

## 0. Executive summary: what changes in the spec

1. **The console gives almost no live progress in the spec's own invocation.** `--format junit` (or any directory or multi-file input) sends `maestro test` down the *suite* path (`TestSuiteInteractor`). On that path stdout gets **no flow-start line and no per-step lines**. It prints only `Waiting for flows to complete...`, **one line per flow after the flow has finished** (`[Passed] Login (12s)` / `[Failed] Checkout (34s) (<error>)`), a suite summary, and a shard box. **[V]** The per-command `RUNNING`/`COMPLETED` lines go to the logger, which means `maestro.log`, and they reach stdout only with the global `--verbose` flag. **[V]** The spec's "Parsed live console events" therefore yields only *flow completed + outcome + duration*.
2. **The best live source is the session `maestro.log`, and its format can be configured.** It is written by a log4j2 `FileAppender`. log4j flushes per event by default **[I]**. The file contains the planned flow list (`Created execution plan: ExecutionPlan(flowsToRun=[...]...)`), the shard-to-device assignment (`[shard 1] Selected device emulator-5554 using port … with execution plan …`), flow start (`Running flow <name>`), and every command start and finish (`<description> RUNNING|COMPLETED|FAILED|SKIPPED|WARNED`). **[V]** Its layout can be overridden with **`MAESTRO_CLI_LOG_PATTERN_FILE`**, a documented env var, so the agent can ask for epoch-ms timestamps, thread names and CRLF-escaped messages. **[V][D]** Maestro's own e2e CI runs `maestro --verbose --no-ansi … test --flatten-debug-output --debug-output=…` **[V]** (`e2e/run_tests` L87).
3. **The artifact layout changed completely in v2.7.0 (2026-07-20).** `commands-(flow).json` and `screenshot-❌-…png` are gone. Each flow now gets its own folder containing `manifest.json` (versioned JSON Schema), `commands.json`, `logs/maestro.log`, `screenshots/step-NNN-<type>-<arg>.png`, and so on. **[V][D]** Write timing matters:
   - The flow folder appears at **flow start**.
   - `logs/maestro.log` grows **during** the flow.
   - Failure screenshot and hierarchy files appear **at the moment a step fails**.
   - `commands.json` and `manifest.json` appear only at **flow end**.
   - The JUnit/HTML report appears only at **suite end, after all shards finish**.
   **[V]** File watching therefore gives live flow start, live step failure and per-flow completion, but no per-step progress for passing steps.
4. **The exit code cannot tell "a test failed" from "Maestro errored".** Both give `1`: `passed == total ? 0 : 1` (`TestCommand.kt` L461), and every exception handler returns `1` (`App.kt` L151-174). picocli usage errors give `2` **[K]**. SIGINT and SIGTERM give the usual JVM `130`/`143` **[I]**. There is **no SIGINT/SIGTERM handling** beyond one shutdown hook that uninstalls the driver. On a signal, the in-flight flow's `commands.json`/`manifest.json` are **not** written and **no JUnit report is written at all**, not even for flows that already finished. **[V]+[I]**
5. **Maestro has no flow-level retries locally.** It has no `--retries` and no retry config. Flow-level retries are a Maestro Cloud feature. **[V][D]** The `retry` *command* is a composite step: `maxRetries` defaults to 1 and is capped at 3. Its attempts show up only as repeated entries in `commands.json` and as `Attempt N failed for retry command` lines in `maestro.log`. JUnit shows one `<testcase>`. **[V]** The spec's "Attempt" entity is therefore always `1` for local runs, unless the CI or our agent re-invokes Maestro.
6. **Sharding semantics, verified in source:**
   - `--shard-split N`: round-robin, flow *i* goes to shard `i % N`. N is capped at the flow count.
   - `--shard-all N`: every shard runs the full plan. N is capped at the device count.
   - Shard *k* uses the *k*-th entry of `--device a,b,c` or, without `--device`, the *k*-th device in adb/simulator enumeration order.
   - **`maestro test` never starts emulators.** With too few connected devices it exits 1 with `Not enough devices connected (…)`.
   - **Without shard flags and with several devices connected, Maestro silently uses the first one.**
   **[V]**
7. **JUnit is non-standard and is only partly correlatable to devices.**
   - Each shard gets one `<testsuite>`, every one carrying the same `name`.
   - `device=` holds the *AVD name* (from `adb emu avd name`), not the serial.
   - `<testcase>` has a non-standard `status` attribute: `SUCCESS`, `ERROR`, or for cloud runs `WARNING`.
   - There are no `errors`/`skipped` counts. `<failure>` has text only and no `message` attribute. Failure text is prefixed `[shard N] `.
   - `file=` (since 2.6.0) gives the CWD-relative flow path, which is the best join key to the plan.
   **[V]**
8. **Flow "skipped" and "interrupted" are never reported by Maestro locally.**
   - Sequential (`executionOrder`) flows that never ran after a failure are simply missing from the report.
   - Flows filtered by tags never enter the plan.
   - **[V]** Both states must be **inferred** as planned minus observed.
9. **Do not pass secrets in `MAESTRO_*` env vars or `-e`.** Every `MAESTRO_*` shell variable and every `-e` value is injected as a `defineVariables` command, and that command is serialized into `commands.json`. **[V]** (`Env.kt` L66-132, open issue #2512.) This bears on how the agent hands a token to flows (§12).
10. **Upstream contribution is feasible.** Maestro already has a clean `OrchestraListener` interface with flow start/end and command start/finish, plus sequence number, depth, timestamps and outcome. The `Orchestra` constructor accepts extra listeners, and a JSON SSE event stream already exists for the MCP viewer. A `--events <file|fd>` NDJSON reporter is roughly a 200-300-line Type-B PR. **[V]** No existing upstream issue requests it, as far as I could find. **[I]**

---

## 1. Version, release cadence, and recent output and reporting changes

**Latest:** CLI **2.12.1**, tagged 2026-11-02 (`maestro-cli/gradle.properties` → `CLI_VERSION=2.12.1`). Tags come in pairs, `v2.12.1` and `cli-2.12.1`. **[V]**

**Cadence (tag dates, from `git for-each-ref`):** 2.0.x (Oct–Nov 2025, several patches); 2.1.0 2026-01-12; 2.2.0 02-20; 2.3.0 03-10; 2.4.0 04-02; 2.5.0 04-27 (2.5.1 04-30); 2.6.0 05-21 (2.6.1 06-12); 2.7.0 07-20; 2.8.0 07-31; 2.9.0 08-26; 2.10.0 08-31; 2.11.0 09-29; 2.12.0 10-26; 2.12.1 11-02. That is **a minor release every 3-5 weeks** plus patches. **[V]** Each `1.x` to `2.x` and `2.n` release has changed output or artifacts, so the agent should **gate parsers on `maestro --version`** and record the version on every suite execution.

**Changes to output and reporting that matter to us** (CHANGELOG.md, **[D]**, cross-checked with `git log` on the files, **[V]**):

| Version | Change | Impact |
|---|---|---|
| 2.0.0 | `--test-output-dir` added. Timestamps added to JUnit and HTML. Java 17 required. | Older agents must not assume the flag exists. |
| 2.1.0 | `--platform`/`--device` on `test`. Custom JUnit properties. `--no-reinstall-driver`. | |
| 2.2.0 | `MAESTRO_DEVICE_UDID`, `MAESTRO_SHARD_ID`, `MAESTRO_SHARD_INDEX` injected into flows. `html-detailed` format. Tags and properties in reports. `--no-ansi` subflow output fixed. | Lets flows self-report shard and device (§12). |
| 2.5.0 | **Android gRPC goes over a direct ADB socket instead of TCP port forwarding.** `XDG_STATE_HOME` honoured for logs. `retryCommand` retries only on `MaestroException`. | No `adb forward` entries to observe (§9-10). |
| 2.6.0 | JUnit `file` attribute. Configurable JUnit `id`/`classname` (`junitId`/`junitClassname` properties). Maestro Viewer added. Rhino removed. | `file` is the plan-to-result join key. |
| **2.7.0** | **"revamp per-flow output into a leaner, flat bundle with a structured manifest, readable step names, device logs, and crash/ANR reports"**. Step screenshot captured *before* every step (only under `--analyze`). `disableAnsi=false` honoured (#3389). | **New artifact layout (§4).** Pre-2.7 parsers break. |
| 2.8.0 | Per-flow `cloud.runId`/`cloud.runUrl` and suite `cloud.uploadId`/`cloud.url` JUnit properties. JUnit `timestamp` truncated to seconds. **Start times recorded for local runs.** Suite duration changed to wall-clock. "Report durations consistently in console output" (whole seconds above 1 s). Hook failure no longer loses artifacts. | Console duration format changed in 2.8.0. |
| 2.9.0 | Negation globs (`!pattern`) in `config.yaml` `flows`. | Re-implementing discovery gets harder. |
| 2.11.0 | `metadata.startedAtEpochMs` on screen recordings in `manifest.json`. | |
| 2.12.0 | `assertScreenshot` diff moved into the bundle. **Per-flow artifact directory names over 255 bytes are shortened with a hash.** | Directory name ≠ flow name, always (§4.3). |

**History of the files we would parse** (`git log --oneline -- <file>`, **[V]**):

- `cli/view/TestSuiteStatusView.kt` (flow-completion and summary lines): 23 commits since 2022-11. The `[Passed]`/`[Failed]` + `N/M Flows Passed|Failed` templates are **unchanged since at least v1.30.0**, the shard prefix was added in v1.39 (sharding, PR #1732, 2024-06), and the duration format changed in 2.8.0 (#3465).
- `cli/runner/TestSuiteInteractor.kt`: 42 commits. The `"${shardPrefix}${desc} RUNNING|COMPLETED"` log templates have existed **since at least v1.30.0**. In 2.7.0 (#3282) they moved into `CliConsoleListener` with the same text.
- `cli/runner/resultview/PlainTextResultView.kt`: 8 commits (2022-12 to 2026-04). `AnsiResultView.kt`: 19 commits.
- `cli/report/JUnitTestSuiteReporter.kt`: 16 commits. It has only *added* attributes and properties since 2024 (status/time 2024-01, timestamp 2025-08, properties 2025-12/2026-02, file 2026-05, cloud 2026-07).
- `maestro-orchestra/.../debug/ArtifactsGenerator.kt`: created 2026-06-25, **11 commits in 4 months**. This is the most volatile surface.

---

## 2. `maestro test` flags, sharding and device assignment

The full option table, copied from the picocli annotations, is in **[`fixtures/maestro-cli/test-command-options-2.12.1.md`](fixtures/maestro-cli/test-command-options-2.12.1.md)**. Highlights:

- `--format`: `JUNIT` | `HTML` | `HTML_DETAILED` (alias `HTML-DETAILED`) | `NOOP` (default) (`cli/report/ReportFormat.kt`). **[V]** The docs page still lists only `JUNIT, HTML, NOOP`. **[D]**
- `--output`: report path. When omitted, the report goes to `report.xml`/`report.html` in the CWD (`TestCommand.kt` L691-699). **[V]**
- `--debug-output`, `--test-output-dir`, `--flatten-debug-output`: see §4.1 for the exact path rules. These differ from the docs.
- `--shards`/`-s`: deprecated alias of `--shard-split`. `--shard-split` and `--shard-all` are mutually exclusive (L256-264). **[V]**
- `--device`/`--udid` (on both `App` and `TestCommand`): comma-separated serials or UDIDs. Every listed device must be connected, otherwise the run fails with `Device X was requested, but it is not connected.` (L378-384). **[V]**
- `--include-tags`/`--exclude-tags`: comma-split and **merged** with `config.yaml` `includeTags`/`excludeTags`. Include matches if **any** tag matches (`WorkspaceExecutionPlanner.kt` L127-135). **[V]**
- `--config`: explicit workspace config. Without it, Maestro uses the first `config.yaml`/`config.yml` found in an input directory. **[V]**
- `-e KEY=VALUE`: repeatable. Values are serialized into `commands.json` (§0.9). **[V]**
- `--no-ansi`/`--no-color` (negatable, both global and on `test`). `--verbose` is **global** (`maestro --verbose test …`). **[V]**
- New since 2.0: `--platform`, `--device` on `test`, `--screen-size`, `--headless` (web), `--[no-]reinstall-driver`, `--analyze`/`--api-url`/`--api-key` (AI beta). There is still **no** `--retries`, `--dry-run`, `--json`, timeout or reporter option. **[V]**

### 2.1 Which code path runs (this decides the console format)

`TestCommand.runShardSuite` (L492-530) **[V]**:

```
isMultipleFiles       = !(flowFiles.size == 1 && !flowFiles.first().isDirectory)
isAskingForReport     = format != NOOP
isReplicatingSingle   = shardAll != null && effectiveShards > 1 && single file
if (isMultipleFiles || isAskingForReport || isReplicatingSingle) -> TestSuiteInteractor   ("suite path")
else if (--continuous)                                             -> TestRunner.runContinuous
else                                                               -> TestRunner.runSingle       ("single path")
```

So **any directory input, any `--format`, or `--shard-all` with one file → suite path.** The spec's example invocation is always on the suite path.

### 2.2 Shard semantics (`TestCommand.handleSessions`, L358-462) [V]

```kotlin
requestedShards = shardSplit ?: shardAll ?: 1
if (requestedShards > 1 && plan.sequence.flows.isNotEmpty()) error("Cannot run sharded tests with sequential execution")
deviceIds = (--device list, each must be connected) ifEmpty { all connected devices [filtered by --platform] }
if (requestedShards - deviceIds.size > 0) -> warn + CliError("Not enough devices connected (n) to run the requested number of shards (m).")
effectiveShards = when {
  onlySequenceFlows -> 1
  shardAll == null  -> requestedShards.coerceAtMost(plan.flowsToRun.size)   // split (or none)
  shardSplit == null-> requestedShards.coerceAtMost(deviceIds.size)         // all
}
chunkPlans = shardAll != null ? N copies of the full plan
           : flowsToRun.withIndex().groupBy { it.index % effectiveShards }    // round-robin
shard k runs on deviceIds[k], port = OS-assigned free port (ServerSocket(0)), coroutine "shard-k"
results = awaitAll(...)  ->  merged JUnit (suites in shard order)  ->  exit 0 iff passed == total
```

Consequences:

- **Accounting.** With `--shard-split`, every planned flow runs exactly once, so the total is the flow count. With `--shard-all N`, every flow runs N times, so the total is N × flows and each run is a separate flow execution. **This confirms the spec's rule that the two need different accounting.** Totals are summed across shards (L448-449).
- **Assignment order** follows the order of `plan.flowsToRun`, which is the `Files.walk` order. That order is **not sorted** and depends on the filesystem (`WorkspaceExecutionPlanner.kt` L54-58, L98-100). **[V]** The agent cannot predict which shard gets which flow from YAML alone. It should read the assignment from `maestro.log` (§3.4).
- **Sequential flows** (`executionOrder`) cannot be sharded at all.
- With `--shard-split 4`, 3 flows and 2 devices, the run fails because the device check uses the *requested* count before capping. **[V]**
- If **one shard's session throws** (for example a driver install failure), `awaitAll` propagates the exception. **No report is written for any shard** and the exit code is 1. **[I]** from coroutine semantics. Phase 0 should check this.
- Console prints `Will split 10 flows across 2 shards (5 flows per shard)` (`approx. ` is added when the split is uneven) or `Will run 2 shards, with all 10 flows in each shard` (L418-429). **[V]** The planned total is therefore on stdout, but **only when a shard flag is given.**

### 2.3 Devices: does Maestro start emulators? [V]

- On the `test` path, `deviceIds` is always non-empty or the command fails, and `PickDeviceInteractor.pickDevice(deviceId)` just looks up the connected device (`device/PickDeviceInteractor.kt` L19-24). The interactive "start or create a device" prompt in `pickDeviceInternal` / `startDevice` is reachable only through other commands, such as `start-device`, Studio and MCP. **`maestro test` does not launch emulators.**
- Device enumeration (`maestro-client/.../device/DeviceService.kt` L239-300) runs `dadb`'s `Dadb.list()`, which uses the adb server (§10). For each emulator it runs `getprop ro.kernel.qemu` and then `adb -s <serial> emu avd name`. The `description` (shown in JUnit `device=` and the shard box) is the **AVD name** when available, otherwise the serial. It also spawns `avdmanager list avd` and `emulator -list-avds`. Enumeration runs **at least 2-3 times per invocation**: in `call()` for analytics, in `handleSessions`, and per shard in `pickDevice`.
- **Without shard flags and with several devices connected, Maestro picks `deviceIds[0]` silently.** Pinning with `--device` is mandatory if the agent wants to know the device without parsing logs.

---

## 3. Console output

### 3.1 ANSI vs plain, and TTY and CI detection [V]

`DisableAnsiMixin.applyCLIMixin` (L41-57):

- `ansiEnabled = <--ansi/--no-ansi if given> ?: isatty(1)`, using jline `CLibrary.isatty`, or `Kernel32` on Windows, with a fallback to `System.console() != null`. **When the agent reads Maestro's stdout through a pipe, ANSI is off automatically.** Passing `--no-ansi` explicitly makes this deterministic.
- **CI detection does not affect the console format.** `CiUtils.getCiProvider()` checks `MDEV_CI`, `APPVEYOR`, `BITBUCKET_BUILD_NUMBER`, `BITRISE_IO`, `BUILDKITE`, `CIRCLECI`, `CIRRUS_CI`, `DRONE`, `GITHUB_ACTIONS`, `GITLAB_CI`, `JENKINS_HOME`, `TEAMCITY_VERSION`, `TF_BUILD` and `CI`. The result only suppresses the Maestro Cloud promotion boxes (`TestCommand.kt` L710-758), goes into `maestro.log` as `CI: <provider>`, and is sent with analytics.
- On the suite path, ANSI mode adds colour escapes only. `Ansi.ansi().fgRed()…` renders to nothing when disabled, so the text is identical. **AnsiResultView** is the full-screen redraw that uses `cursor(0,0)` and erase-screen (`AnsiResultView.kt` L228-262). It is used **only on the single-flow path with a TTY**. It cannot be parsed and must be avoided.
- An open issue reports ANSI output making runs ~37% slower on Windows (#3721). This is another reason to force `--no-ansi`. **[D]**

### 3.2 Suite path (what the spec's invocation prints) [V]

All of these are `println` on stdout:

| When | Template (source) | Example (reconstructed, not captured) |
|---|---|---|
| Very first run on a machine only | `"Anonymous analytics enabled. To opt out, set MAESTRO_CLI_NO_ANALYTICS environment variable to any value before running Maestro.\n"` (`analytics/Analytics.kt`) | |
| Deprecated `--shards` | `PrintUtils.warn("--shards option is deprecated …")` | |
| Web flows present | `"Web support is in Beta. We would appreciate your feedback!\n"` | |
| Sharding | `"Will split $flowCount flows across $effectiveShards shards (${prefix}$flowsPerShard flows per shard)"` / `"Will run $effectiveShards shards, with all $flowCount flows in each shard"` / over-request warning `"Requested N shards, but it cannot be higher than the number of flows (M). Will use K shards instead."` | `Will split 5 flows across 2 shards (approx. 3 flows per shard)` |
| Not in CI, more than 5 flows, at most once a day | cloud promotion box | |
| Per shard, at suite start | `PrintUtils.message("${shardPrefix}Waiting for flows to complete...")` (`TestSuiteInteractor.kt` L66). `message()` prepends `\n`. | `[shard 1] Waiting for flows to complete...` |
| **Per flow, after it finishes** | `TestSuiteStatusView.showFlowCompletion`: `<shardPrefix>[<Passed\|Failed>] <flowName> (<duration>)[ (<error message>)]` (`view/TestSuiteStatusView.kt` L52-70) | `[shard 2] [Failed] Checkout (34s) (Element not found: Text matching regex: Pay)` |
| Per shard, after the last flow | `"${shardPrefix}${failed}/${total} Flow(s) Failed"` or `"${shardPrefix}${passed}/${total} Flow(s) Passed in <duration>"` (+ `"N Flows Canceled"`; cloud only) (L72-120) | `1/3 Flow Failed` (note: `flowWord` counts the *failed* number) |
| Sharded runs, at the end | box: `Passed: P/T` + `[ <deviceDescription> ] - p/t` per shard (`TestCommand.kt` L683-689) | |
| End, stderr | update-available box (unless `MAESTRO_DISABLE_UPDATE_CHECK=true`) | |
| End | "Analyze with Ai" box (unless `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED`) | |
| Fatal error, stderr | red error message plus `"\nThe stack trace was:"` and the stack, except for `CliError` (`App.kt` L151-174) | `Not enough devices connected (1) to run the requested number of shards (2).` |

Maestro's own tests pin the per-flow format: `"[Passed] flow1 (2s)"`, `"[Failed] flow2 (60ms)"`, `"1/3 Flow Failed"`, `"2/2 Flows Passed"` (`maestro-cli/src/test/kotlin/maestro/cli/cloud/CloudInteractorTest.kt` L732-816). **[V]**

Parsing hazards **[V]**:

- The duration is a Kotlin `Duration.toString()`, rounded to whole seconds above 1 s (`util/TimeUtils.kt`), so `12s`, `1m 5s`, `450ms`. This format changed in 2.8.0.
- The error text comes from `debugOutput.exception?.message`. It **can contain newlines**, and it is **null when the failure was a non-`MaestroException`** (that message goes only into JUnit).
- The flow name comes from the user's `name:` and can contain anything, including `)` or `[`.
- **There is no "flow started" line and no device line on this path.**

### 3.3 Single-flow path (one file, no `--format`) [V]

`TestRunner.runSingle` → `MaestroCommandRunner.runCommands`, re-rendered on every state change.

- **`PlainTextResultView`** (non-TTY or `--no-ansi`) prints each item once:
  - `Running on <device description>`
  - ` > Flow <flowName>`
  - `  > On Flow Start` / `  > On Flow Complete`
  - composite start `"<desc>..."` and end `"<desc>... COMPLETED"`
  - simple commands as `print("<desc>...")`, then later `println(" COMPLETED|FAILED|SKIPPED|WARNED")`
  - insights as `Warning:` blocks
  (`PlainTextResultView.kt` L39-121). At the end it prints the error and debug message, then `==== Debug output (logs & screenshots) ====` and the path (`TestCommand.kt` L677-681).
- **Line-reader caveat [I].** A simple command's start text is printed *without* a newline, so a line-based reader sees it only when the command completes. Whether the partial text is flushed earlier depends on `System.out` buffering, which is autoflush on newline in the JDK **[K]**.
- **`AnsiResultView`** (TTY): full-screen frames with `✅ ❌ ⏳ 🔲 ⚠️ ⚪️` symbols. Not parseable.
- This path has richer live output, but it runs one flow per JVM. Re-invoking Maestro per flow costs a JVM start-up, device enumeration and a driver (re)install each time (`--reinstall-driver` defaults to true), and gives up Maestro's own sharding. **Not recommended.**

### 3.4 Machine-readable live output: what exists

**There is no JSON event stream, reporter plugin API, `--json` flag or listener SPI on `maestro test`.** **[V]** The usable signals are below.

**(a) Session `maestro.log`.** log4j2 `FileAppender` with the root logger at `ALL` (`maestro-client/src/main/java/maestro/debuglog/LogConfig.kt`).

- Default pattern: `%d{HH:mm:ss.SSS} [%5level] %logger.%method: %msg%n`. It can be overridden with `MAESTRO_CLI_LOG_PATTERN_FILE` (present since 2024-09, commit `6d4b2ab9`). **[V][D]**
- log4j2 `FileAppender` defaults to `immediateFlush=true`, so lines appear as they are logged **[I]/[K]**.
- Key lines **[V]**:

  | Template (source) | Meaning |
  |---|---|
  | `Created execution plan: $executionPlan` (`maestro-orchestra/src/main/java/maestro/orchestra/workspace/WorkspaceExecutionPlanner.kt` L179) | The plan. A Kotlin `data class` `toString()`: `ExecutionPlan(flowsToRun=[/abs/a.yaml, …], sequence=FlowSequence(flows=[…], continueOnFailure=…), workspaceConfig=WorkspaceConfig(…))`. |
  | `[shard ${i+1}] Selected device $deviceId using port $port with execution plan $executionPlan` (`TestCommand.kt` L475) | Shard → device serial → that shard's own planned flows. |
  | `$shardPrefix Running flow $flowName` (`TestSuiteInteractor.kt` L181) | **Flow start.** Note the double space after the shard prefix, or a leading space when there are no shards. |
  | `${shardPrefix}${cmd.description()} RUNNING\|COMPLETED\|FAILED\|SKIPPED\|WARNED\|PENDING` (`cli/runner/CliConsoleListener.kt`) | **Step start and finish**, nested ones included. Hooks are logged as ordinary commands. |
  | `[Command execution] CommandFailed: <msg>` / `CommandWarned` / `CommandSkipped` (`Orchestra.kt` L388-403) | Failure detail. |
  | `Attempt N failed for retry command` / `Max retries (N) reached. Commands failed.` (`Orchestra.kt` L1007-1012) | Command-level retry. |
  | `${shardPrefix}Failed to complete flow` + stack (`TestSuiteInteractor.kt` L212) | Flow ended with an exception. |
  | `onFlowComplete hook failed: …`, `JsConsole: <msg>` | Hook failure and JS `console.log`. |
  | `---- System Info ----` block with `Maestro Version: …`, `CI: …`, OS, Java (`TestDebugReporter.kt` L161-181) | Version detection without running `--version`. |

  The command `description()` is user-influenced (the `label:` field replaces it), so match the status keyword as a **suffix**.

**(b) `--verbose`.** The same log events also go to **stdout**, with pattern `%highlight{[%5level]} %msg%n`, overridable via `MAESTRO_CLI_LOG_PATTERN_CONSOLE`. **[V]**

- Interleaved with normal stdout.
- Volume is unknown: the root logger is at `ALL`, and only grpc, netty and ktor are muted. **[I]**
- `%highlight` may still emit ANSI codes under `--no-ansi` **[I]**.

**(c) The filesystem (§4).** Flow folder creation, per-flow `logs/maestro.log` growth, failure screenshots, `commands.json` and `manifest.json`.

**(d) `~/.maestro/sessions`.** Maestro writes one `key=value` line per Maestro session, `<PLATFORM>_<deviceId>_<uuid>=<epochMs>`, and refreshes it every 5 s. Entries older than 21 s count as dead (`cli/session/SessionStore.kt`, `cli/db/KeyValueStore.kt`, `cli/session/MaestroSessionManager.kt` L93-131). **[V]** This is a live "Maestro is attached to device X" signal. It is per user and home directory, not per invocation, and the file is locked while written.

**(e) MCP viewer SSE stream.** `cli/mcp/viewer/McpViewerServer.kt` serves `/api/events/stream` with typed JSON events (`maestro.flow_state` with per-command status and depth). **It exists only in `maestro mcp`, not in `maestro test`.** It is relevant as upstream precedent (§12). **[V]**

---

## 4. Artifacts: layout, write timing, schemas

### 4.1 Where things go (`cli/report/TestDebugReporter.kt` L187-226, `TestCommand.kt` L249-289) [V]

`<ts>` = `yyyy-MM-dd_HHmmss`, captured once at `install()`. `$STATE` = `$XDG_STATE_HOME/maestro` if that variable is set, otherwise `~/.maestro`.

`install()` places the session `maestro.log` **before** `--test-output-dir` is resolved. The per-flow bundles go under the path resolved *after* it. The two can therefore land in different trees:

| Flags | Session `maestro.log` | Per-flow bundles `<flow>/…` (and `ai-*.json`, `ai-report-*.html` from `--analyze`) |
|---|---|---|
| none | `$STATE/tests/<ts>/maestro.log` | `$STATE/tests/<ts>/` |
| `--test-output-dir T` (or `testOutputDir:` in `config.yaml`) | `$STATE/tests/<ts>/maestro.log` | `T/<ts>/` |
| `--debug-output D` | `D/.maestro/tests/<ts>/maestro.log` | `D/.maestro/tests/<ts>/` |
| `--debug-output D --test-output-dir T` (**the spec's example, with D = T**) | `D/.maestro/tests/<ts>/maestro.log` | `T/<ts>/` → **two different subtrees even when D = T** |
| `--flatten-debug-output --debug-output D` | `D/maestro.log` | `D/` (**`--test-output-dir` ignored**) |
| `--flatten-debug-output` alone | `~/maestro.log` (`user.home`!) | `~/` |

Unit tests confirm the flatten, testOutputDir-base and custom-root rules (`maestro-cli/src/test/kotlin/maestro/cli/report/TestDebugReporterTest.kt` L48-120). **[V]**

The docs disagree in two places **[D]**:

- They say that with `--debug-output` only, artifacts go "to the default test output tree". The code puts them under `D/.maestro/tests/<ts>/`.
- They say that when both flags point to the same directory, everything goes together. The code splits it into two subtrees.
- The `--test-output-dir` help text says "written directly into it", but the code inserts `<ts>/`.

**Phase 0 must confirm which is true at runtime.**

Other behaviour:

- Without `--flatten-debug-output`, Maestro deletes sibling directories older than 14 days (`deleteOldFiles`).
- **With flatten, re-using a directory makes flow folders collide**, giving `login-2` and so on (`resolveFlowDir`). Use a fresh empty directory per invocation.
- Also written: the `DebugLogStore` directory in the user log dir. On Linux that is `$XDG_CACHE_HOME`, usually `~/.cache/maestro/logs/<ts>_<pid>/`; on macOS `~/Library/Logs/maestro/`, via AppDirs **[K]**. It holds a java.util.logging `maestro.log`, `system_info.log` and redirected child-process logs, and is zipped at exit (`DebugLogStore.finalizeRun`). The newest 6 zips are kept. iOS adds `xctest_runner_*.log` in the session directory. **[D]**

**Recommended layout for the agent:** `maestro … test --flatten-debug-output --debug-output <fresh-dir> --format junit --output <fresh-dir>/report.xml …`. Everything then sits in one directory, which is what Maestro's own e2e harness does.

### 4.2 Per-flow bundle (`maestro-orchestra/src/main/kotlin/maestro/orchestra/debug/BundleLayout.kt`) [V]

```
<flowDir>/                     # sanitized flow name ("/"→"_"), "-shard-N" when sharded, "-N" on collision, >255B → truncated + "-<8 hex sha256>"
  manifest.json                # $schema https://storage.googleapis.com/maestro-schemas/artifact-manifest/v1.schema.json
  commands.json
  logs/
    maestro.log                # per-flow log4j capture of maestro.* / MAESTRO loggers only
    logcat.txt (name from DeviceArtifactFiles.LOGCAT), crash/ANR reports
  takeScreenshot/              # takeScreenshot command output
  startRecording/              # startRecording command output
  screenshots/step-<NNN>-<type>[-<arg>].png   # CLI: only failed/warned steps (+ every step and final.png under --analyze)
  screen-hierarchy/step-<NNN>-<type>[-<arg>].json  # failed/warned visible leaf steps
  assert-screenshot/step-<NNN>-…-diff.png
  screen-recording.mp4         # --analyze only (captureFullArtifacts)
  ai-analysis/step-<NNN>.png
```

`NNN` = 1-based sequence number, zero-padded to 3 digits. The slug is the command class name minus `Command` plus one argument, for example `step-005-tapOnElement-Login` (`StepArtifactNaming.kt`). **[V]**

### 4.3 When each file is written. This decides whether file-watching is live. [V]

Sources: `TestSuiteInteractor.runFlow` (L157-275), `Orchestra.runFlow` (L228-320), `ArtifactsGenerator` (whole file).

| Order | Event | File effect | Live? |
|---|---|---|---|
| 1 | Suite start (`install()`) | session `maestro.log` created and appended continuously | **live** |
| 2 | Flow start, before any command | `createFlowDir()` → **flow directory created** | **live: flow-started signal**, with sanitized name and shard suffix |
| 3 | `onFlowStart` listener | `logs/maestro.log` created (`ScopedLogCapture`) and appended during the flow. **`logcat -c` clears the device log buffer** (`AndroidDriver.startDeviceLogCapture`). | **live** |
| 4 | Each command start | only in-memory metadata (CLI). Under `--analyze`, a pre-command `screenshots/step-NNN-….png` (via `.tmp` + atomic move). | not live (CLI default) |
| 5 | Command FAILED/WARNED | `screen-hierarchy/step-NNN-….json` (about 1 s), then `screenshots/step-NNN-….png`. Composite parents (`runFlow`, `retry`) that fail as a result get their own screenshot. | **live: step-failure signal** |
| 6 | `takeScreenshot` / `startRecording` commands | written into `takeScreenshot/` / `startRecording/` as they run | live |
| 7 | Flow body done → `onFlowComplete` hooks → JS engine closed → crash check | | |
| 8 | `onFlowEnd` | **`commands.json`** written, then `logs/` logcat dump (`logcat -v time -d`) plus crash/ANR, then **`manifest.json`** | **per-flow completion** (`manifest.json` is the last file written) |
| 9 | Back in `TestSuiteInteractor` | **console `[Passed]/[Failed]` line printed** after the bundle is complete | live |
| 10 | Shard done | console summary. `ai-(<flow>).json` + `ai-report-*.html` if there were AI outputs (`TestDebugReporter.saveSuggestions`) | |
| 11 | **All shards done** | **JUnit/HTML report written once** (`TestCommand.kt` L457, `saveReport`), not atomically (okio sink, no temp + rename) | **end only** |
| 12 | JVM exit | `DebugLogStore` dir zipped | |

Caveats:

- **[V]** `commands.json` is written with `Files`/Jackson directly (no temp + rename), and `manifest.json` with `writeText`. Treat **`manifest.json`'s appearance as the per-flow "bundle complete" marker**, wait for a quiet period, and re-read `commands.json` defensively.
- **[I]** In sharded runs each flow's `ScopedLogCapture` appender attaches to the shared `maestro` logger and filters only by logger name. **Concurrent shards' lines will likely appear in each other's per-flow `logs/maestro.log`.** Attribute lines by the `[shard N]` prefix (only some lines carry it) or by thread name (§14).
- **[V]** On cancellation, `onFlowComplete` is skipped, but `onFlowEnd` still runs inside the `finally`. That happens only for *coroutine* cancellation. A SIGINT does not cancel coroutines (§8), so the in-flight bundle is left without `commands.json`/`manifest.json`.

### 4.4 `commands.json` schema (serialized Kotlin classes) [V]

Writer: `TestOutputWriter.saveCommands` uses Jackson with `NON_NULL` + `NON_EMPTY` and a pretty printer (`maestro-orchestra/.../debug/TestOutputWriter.kt`). It writes a **JSON array, one element per *execution*** (`FlowDebugOutput.executedSteps`), in start order. A retried or repeated command appears once per attempt, under the same `command` with a different `sequenceNumber`. Each element is `CommandDebugWrapper(command, metadata)`:

```jsonc
[
  {
    "command": {                         // maestro.orchestra.MaestroCommand: exactly one non-null field, e.g.
      "tapOnElement": { "selector": { "textRegex": "Login", ... }, "label": "...", "optional": false, ... }
      // other keys: launchAppCommand, assertConditionCommand, runFlowCommand, retryCommand, repeatCommand,
      // defineVariablesCommand (carries the env map: secrets!), applyConfigurationCommand, runScriptCommand, ...
    },
    "metadata": {                        // maestro.orchestra.debug.CommandDebugMetadata
      "status": "COMPLETED",             // PENDING | RUNNING | COMPLETED | FAILED | WARNED | SKIPPED
      "timestamp": 1762182192345,        // epoch ms at onCommandStart
      "duration": 812,                   // ms, finishedAt - startedAt
      "error": { "message": "...", "debugMessage": "..." },   // Throwable slimmed by mixin; no stack, no hierarchy
      "sequenceNumber": 4,               // 0-based, monotonic across the whole flow incl. nested + hooks
      "depth": 1,                        // 0 = flow top level; +1 per runFlow/repeat/retry
      "evaluatedCommand": { ... },       // command after ${} / JS evaluation (labels, interpolated text: may hold secrets)
      "artifacts": [ { "type": "SCREENSHOT", "path": "screenshots/step-005-tapOnElement-Login.png" } ]
    }
  }
]
```

Values are illustrative; the field set comes from `FlowDebugOutput.kt`, `CommandStatus.kt` and the tests in `ArtifactsGeneratorTest.kt` L73-1010.

- `command` back-references are `@JsonIgnore`.
- Empty `artifacts` are omitted (test L742-754). Under `NON_EMPTY`, whether `sequenceNumber: 0` / `depth: 0` are emitted needs **Phase 0** confirmation.
- Hooks (`onFlowStart`/`onFlowComplete` commands) and `defineVariables` appear as ordinary entries. **Step counts must filter them**, for example drop `defineVariablesCommand`/`applyConfigurationCommand`, and mark hook ranges by position.

### 4.5 `manifest.json` [V]

`ArtifactManifest { entries: [ArtifactEntry { kind, format?, relativePath, count?, sizeBytes?, metadata{} }] }` with a leading `$schema`. The schema is published at `https://storage.googleapis.com/maestro-schemas/artifact-manifest/v1.schema.json`; the source copy is `maestro-orchestra-models/src/main/resources/maestro/orchestra/artifact-manifest/v1.schema.json`.

- `kind`: `SCREENSHOT, TAKE_SCREENSHOT, SCREENSHOT_DIFF, SCREEN_RECORDING, START_SCREEN_RECORDING, SCREEN_HIERARCHY, COMMAND_METADATA, MAESTRO_LOG, DEVICE_LOG, CRASH_REPORT, ANR_REPORT, AI_ANALYSIS`.
- `format`: `PNG, MP4, JSON, TXT, HTML`.
- The contract states that readers must treat `kind` as an open string, and that breaking changes get a new `vN` URL.
- **This is the most stable machine-readable artifact contract Maestro offers.** The agent should drive artifact inventory from it, not from directory scanning.

---

## 5. Reports

### 5.1 JUnit XML (`cli/report/JUnitTestSuiteReporter.kt`; expected strings in `maestro-cli/src/test/kotlin/maestro/cli/report/JUnitTestSuiteReporterTest.kt`) [V]

```xml
<?xml version='1.0' encoding='UTF-8'?>
<testsuites>
  <testsuite name="Test Suite" device="Pixel_7_API_34" tests="2" failures="1" time="552.743" timestamp="2026-11-03T10:15:42">
    <properties><property name="cloud.uploadId" value="…"/></properties>           <!-- cloud only -->
    <testcase id="Flow A" name="Flow A" classname="Flow A" file=".maestro/flow_a.yaml" time="421.573" timestamp="2026-11-03T10:15:43" status="SUCCESS"/>
    <testcase id="Flow B" name="Flow B" classname="Flow B" file=".maestro/sub/flow_b.yaml" time="131.846" timestamp="…" status="ERROR">
      <properties>
        <property name="testCaseId" value="TC-001"/>      <!-- flow `properties:` (minus junitId/junitClassname) -->
        <property name="tags" value="smoke, critical"/>   <!-- flow `tags:` joined ", " -->
      </properties>
      <failure>[shard 2] Element not found: …</failure>
    </testcase>
  </testsuite>
</testsuites>
```

- `testsuite@name`: `--test-suite-name`, falling back to `"Test Suite"`. It is the same for every shard.
- `testsuite@device`: `device.description` (the AVD name or the serial). It is omitted when null.
- `tests`/`failures` are counted per shard. There are **no `errors` or `skipped` attributes.**
- `time`: seconds as `Double.toString()`. Since 2.8.0 the suite `time` is wall-clock.
- `timestamp`: **local time with no zone**, `ISO_LOCAL_DATE_TIME` truncated to seconds, in the JVM's default zone. The agent must record the runner's time zone.
- One `<testsuite>` per shard, **in shard index order**. `mergeSummaries` concatenates `awaitAll` results in order (`TestCommand.kt` L701-708). Test cases are in execution order. Without sharding there is a single `<testsuite>`.
- `testcase@id`/`name`/`classname`: the flow `name:` or the file name without extension. `id` and `classname` can be overridden by the `junitId`/`junitClassname` flow properties (2.6.0).
- `testcase@file`: the CWD-relative flow path, or absolute when outside the CWD (`toCwdRelativeOrAbsoluteString`), since 2.6.0. **This is the reliable key.**
- `status`: the `FlowStatus` enum name. Locally it is only `SUCCESS` or `ERROR`. Cloud adds `WARNING`/`CANCELED`/`STOPPED`/….
- `<failure>`: text only. The content is `shardPrefix + (ErrorViewUtils.exceptionToMessage(e) ?: exception.message ?: "Unknown error")`. Non-Maestro exceptions are rendered as a **full stack trace** (`view/ErrorViewUtils.kt`). There is no `message=` and no `type=`.
- With `--shard-all`, the same flow appears once per shard, with the same `id`/`name`, inside different `<testsuite>` elements.
- **The device serial is not in the report.** Map suite *k* to `[shard k+1] Selected device <serial>` in `maestro.log`, or to the `--device` list order.

### 5.2 HTML (`cli/report/HtmlTestSuiteReporter.kt`) [V]

- A Bootstrap page, with CSS from the jsdelivr CDN.
- One "Flow Execution Summary" card per suite: Test Result, Duration, Start Time, then Total, Failed and Successful counts, then a list of failed flows.
- One collapsible card per flow, titled `<name> : <status>`, with duration, start time, tags, properties and failure message.
- `--format html-detailed` adds the steps: numbered `description`, `status` and `duration` (`<1ms` / `NNNms` / `N.Ns`) from `commands.json` metadata (`TestSuiteInteractor.kt` L230-254).
- It is written at the same moment as JUnit and holds less information. **Do not parse HTML. Use JUnit plus `commands.json`.**

---

## 6. Flow discovery and the plan

`WorkspaceExecutionPlanner.plan` (`maestro-orchestra/src/main/java/maestro/orchestra/workspace/WorkspaceExecutionPlanner.kt`) [V]:

1. **Single file input** → the plan is that file. `config.yaml` is read only when `--config` is given.
2. **Directories** are walked recursively (`Files.walk`). A file is a candidate if it is `*.yaml`/`*.yml`, is not named `config`, is not the resolved config file, and is **not detected as a workspace config by content**: no `\n---` and only known config keys (`workspace/Filters.kt`, since 2.5.0).
3. `config.yaml` `flows:` globs are matched against `<inputDir>/<glob>`. The **default is `["*"]`, top-level files only.** Since 2.9.0, `!negation` globs are supported. If only negations are given, the run fails with an error.
4. Every candidate's YAML is parsed (`validateFlowFile`). Tags are filtered: include matches on any tag, exclude matches on any tag, and CLI and config values are merged.
5. `executionOrder.flowsOrder` (by flow name) takes the matching flows out into `sequence` (`ExecutionOrderPlanner`). `continueOnFailure` defaults to `true`. With `false`, the rest of the sequence is **not run and not reported** (`TestSuiteInteractor.kt` L82-89). The normal flows still run afterwards.
6. `addMedia` paths are validated, then `logger.info("Created execution plan: …")`.

**Top-level vs `runFlow` subflows.** Maestro has no marker for this. A file is top-level only because it matches the `flows` globs. Subflows usually live in subdirectories, so the default `*` glob excludes them. A `flows: ["**"]` config would turn subflow files into top-level flows. At runtime a `runFlow` is a composite *command*, `RunFlowCommand`, at `depth+1` in `commands.json`, and it is logged as `Run <file>... RUNNING`. **It is never a separate JUnit `testcase`.** **[V]** This confirms the spec: "A nested `runFlow` is a step/subflow".

**`continueOnFailure`** exists only under `executionOrder` (sequence). There is no suite-wide continue or stop option. Normal flows always all run. **[V]**

**`onFlowStart`/`onFlowComplete`** are per-flow config keys (`Orchestra.kt` L251-301, docs **[D]**):

- `onFlowStart` runs before the body. If it fails, the body is skipped and the flow is marked failed.
- `onFlowComplete` always runs, except on coroutine cancellation. If it fails, the flow is marked failed. Its commands are logged and recorded like body commands.
- There is **no workspace-level (global) hook.**

**Getting the planned flow list without running:** there is **no `--dry-run` or list command** **[V]**. The options are:

- **(a)** Parse `Created execution plan:` from `maestro.log`. It appears within seconds of start, before devices are touched. This is exact, because it is Maestro's own result, including tags and negation globs.
- **(b)** The `Will split/run …` stdout line, which gives counts and appears only with sharding.
- **(c)** A CI-supplied manifest, as the spec requires.
- **(d)** Re-implementing the planner in Rust. Possible, but drift-prone: `Files.walk` order, content-based config detection, glob semantics of Java `PathMatcher`, YAML parsing of `name`/`tags`. **The spec's warning is correct.** Option (a) gives *exact* planned totals live, and is the strongest finding in this report for the "planned" state.

---

## 7. Retries

- **Flow-level, local CLI:** none. No `--retries`, no config key. `WorkspaceConfig.disableRetries` is `@Deprecated("not supported now by default on cloud")` (`maestro-orchestra-models/.../WorkspaceConfig.kt` L14). **[V]**
- **Flow-level, Maestro Cloud:** "smart retries" for suspected flakes and infrastructure-triggered retries on another machine. These do not happen locally. **[D]**
- **The `retry` command** (`Orchestra.retryCommand`, L995-1018): `maxRetries` defaults to 1 and is capped at 3 (`MAX_RETRIES_ALLOWED`, L1907). It retries only on `MaestroException` (since 2.5.0). **[V][D]**
  - Each attempt re-runs the subcommands through `runSubFlow`. Subcommands are reset to PENDING (`onCommandReset` → `PENDING` log line) and get new `sequenceNumber`s.
  - It shows up as extra `commands.json` entries (one per execution, test L930-950), as `Attempt N failed for retry command` (ERROR level with stack) and `Max retries (N) reached…` in `maestro.log`, and as an insight warning that is not printed on the suite path.
  - **JUnit shows one `<testcase>` with the final outcome. Nothing indicates a retry happened.**
- Implication for the data model: a **command retry is not a test attempt**, as the spec says. A flow **Attempt > 1 can only come from outside Maestro**: a CI job re-run, or the agent invoking Maestro again for the failed flows. The agent must create the Attempt itself in those cases.

---

## 8. Exit codes and signal handling

| Situation | Exit code | Source |
|---|---|---|
| All flows passed (`passed == total`) | `0` | `TestCommand.kt` L461 [V] |
| Any flow failed | `1` | L461 [V] |
| Single-flow path failed | `1` | L596-597 [V] |
| `CliError` (no devices, bad device, bad config, no flows match tags, …) | `1` (red message, no stack) | `App.kt` L151-174 [V] |
| Any other exception (driver install failure, `error(…)`, IllegalState, shard crash) | `1` (message plus stack trace on stderr) | same [V] |
| picocli parse or usage error (unknown option, bad `--format`) | `2` (picocli default `ExitCode.USAGE`) | [K] |
| Uncaught `Throwable` escaping `main` (e.g. OOM before picocli) | `1` (JVM default for uncaught exceptions) | `App.kt` L215-218 [I] |
| SIGINT / SIGTERM | `130` / `143` (JVM: 128 + signal after shutdown hooks) | [I] |
| SIGKILL | `137` (no cleanup) | [K] |

**A flow failure cannot be told apart from a Maestro or infra error by the exit code alone.** To classify, combine:

- report presence (absent → error before or during the suite),
- the stderr `The stack trace was:` marker,
- the `CliError` message patterns.

**Signal handling [V]:**

- There are **no `sun.misc.Signal` handlers**. The only shutdown hook in the `test` path is in `MaestroSessionManager.newSession` (L124-131), one per shard. It:
  - cancels the heartbeat,
  - deletes the `~/.maestro/sessions` entry,
  - calls `ScreenReporter.reportMaxDepth()`, a **network call** to `/maestro/screen` when the depth tracker recorded anything,
  - if no other Maestro session uses the device, calls `session.close()`. For Android that resets the proxy and mock location, **uninstalls `dev.mobile.maestro` and `dev.mobile.maestro.test` when `reinstallDriver`** (the default), and closes the instrumentation (`AndroidDriver.close()` L170-195).
- **What does not happen on SIGINT/SIGTERM [I]:**
  - Coroutines are not cancelled, so the in-flight flow gets no `onFlowComplete`, no `commands.json` and no `manifest.json`.
  - `saveReport()` never runs, so **no JUnit for the flows that already finished**. Their bundles and console lines are already on disk and stdout.
  - `DebugLogStore.finalizeRun()` never runs, so the log directory is not zipped.
  - The flow thread races with `session.close()` until the JVM halts.
- **Agent policy:**
  1. Forward SIGTERM (or SIGINT) to the JVM PID.
  2. Wait for a bounded time; the shutdown hook needs a few seconds for adb uninstall.
  3. Then SIGKILL.
  4. Build the final state from the console lines, the `manifest.json` files present, the `maestro.log` tail, and "flow dir without `manifest.json`" = interrupted.
  5. Never synthesize a JUnit file.
- After SIGKILL, the stale `~/.maestro/sessions` entry ages out after 21 s and the driver APKs stay installed. The next run reinstalls them anyway.

---

## 9. Process tree (Android, Linux/macOS host)

Launcher: `maestro` is the Gradle `application` start script (`maestro-cli/build.gradle.kts` L27-36, L138-160). Maestro patches it to first run `"$JAVACMD" -classpath "$APP_HOME"/bin/*.jar JvmVersion` (a short-lived Java version check) and then **`exec "$JAVACMD" …`**. **[V]** The PID the agent spawns *becomes* the main JVM (`maestro.cli.AppKt`, JVM arg `--enable-native-access=ALL-UNNAMED`). JVM options can be injected through the start script's `JAVA_OPTS`/`MAESTRO_OPTS` **[K]** (Gradle template) or `JAVA_TOOL_OPTIONS` **[K]**.

| Process | Lifetime | Spawned by | Source |
|---|---|---|---|
| `java … JvmVersion` | under 1 s, at start | start script | build.gradle.kts L126-130 [V] |
| **`java … maestro.cli.AppKt test …`** (same PID as the spawned `maestro`) | whole run. All shards are threads inside this one JVM. | exec | [V] |
| `flutter --no-version-check --version --machine` | start (system-info logging), only if `flutter` is on PATH | `cli/util/EnvUtils.kt` L85-90 [V] | can be slow |
| `xcodebuild -version` | start (macOS) | `cli/util/IOSEnvUtils.kt` [V] | |
| `which adb`; `adb -P 5037 start-server` | only if no adb server is listening. **The adb server daemonizes and outlives Maestro.** | dadb `AdbBinary.ensureServerRunning` [V] | |
| `avdmanager list avd` (itself a JVM tool), `emulator -list-avds` | every device enumeration (2-3× per run) | `DeviceService.kt` L297, L338 [V] | noticeable CPU at start |
| `adb -s <serial> emu avd name` | per emulator, per enumeration | `DeviceService.kt` L264 [V] | |
| `uname`, `printenv`, `sysctl` | occasionally (arch/WSL detection) | `EnvUtils` [V] | |
| **adb server** (pre-existing, shared) | persistent | – | carries all dadb traffic, so its CPU is not attributable per run [I] |
| On the device (guest, not host): `am instrument -w -m -e debug false -e class 'dev.mobile.maestro.MaestroDriverService#grpcServer' -e port <P> dev.mobile.maestro.test/androidx.test.runner.AndroidJUnitRunner` → the instrumentation process hosting the gRPC server; `dev.mobile.maestro` app; `screenrecord` during recordings; `logcat -c` / `logcat -v time -d` per flow | per session / per flow | `AndroidDriver.kt` L123-134, L861-871 [V] | guest metrics only |

- **No host-side port forwards.** Since 2.5.0 gRPC runs over a dadb stream to `tcp:<port>` on the device (`AndroidDeviceConnection.buildChannel`, `AdbSocketFactory`). **[V]** The per-shard "port" is a device-side port, picked as a free *host* port number with `ServerSocket(0)`.
- **Attribution [I]:** host CPU and memory for Maestro = the main JVM PID (plus short-lived children). The emulator process (`qemu-system-*`) is separate and pre-existing. The agent should map emulator PIDs to serials through the console port or AVD name, not through Maestro.

---

## 10. Device connection and running a second adb client next to Maestro

- **Library:** `dev.mobile:dadb` **2.0.0** (`gradle/libs.versions.toml`). It is the only code that creates a `Dadb` (`maestro-client/src/main/java/maestro/android/AndroidDeviceConnection.kt` L376-466). **[V]**
- **Transport:** `Dadb.list("localhost")` → `AdbServer.listDadbs` → `host:devices` on **the adb server at 5037**, then `host:transport:<serial>` per device (dadb `AdbServer.kt` L64-86). **[V]** It falls back to direct adbd connections on the emulator ports only if the server lists nothing (`Dadb.kt` L255-272). A second listing on adb server port 5038 exists only in a device-discovery helper.
- **gRPC driver:** the on-device server is reached through an adb stream, with a 120 s deadline per call and 2-minute keepalive (`AndroidDeviceConnection.kt` L83-97). If the stream dies (UNAVAILABLE or DEADLINE_EXCEEDED), Maestro throws `DeviceConnectionException`/`DeviceUnreachableException` and the flow fails. **[V]**
- **Our agent querying adb at the same time:** the adb server multiplexes clients, and Maestro uses ordinary server transports, so read-only `adb -s X shell getprop`, `dumpsys`, `adb devices` and `adb emu avd name` should coexist safely. **[I]** Things the agent **must not** do:
  - **`adb kill-server` / `adb reconnect` / `adb start-server -a`, or restarting adb.** That severs every Maestro stream and fails the flow. **[I]**
  - **`logcat -c`.** Maestro itself clears logcat at every flow start (`startDeviceLogCapture`). If the agent wants continuous logcat, it must stream (`logcat -v epoch`), not poll with `-d`, and expect a clear at every flow start. **[V]** for Maestro's behaviour.
  - **`uiautomator dump` or other UiAutomation clients** for "current activity". Maestro's instrumentation holds the UiAutomation connection, and a second one can break it. Use `dumpsys activity activities | grep -E 'mResumedActivity|topResumedActivity'` or `dumpsys window` instead. **[K]**, and Phase 0 should check it.
  - `pm uninstall` of `dev.mobile.maestro*`, `am force-stop dev.mobile.maestro*`, `screenrecord` (Maestro may be recording).
  - Heavy, frequent `dumpsys meminfo` or `top` on the guest. These add guest CPU. Keep the spec's "No unnecessary ADB commands" rule.

---

## 11. Telemetry, analytics and Cloud vs local

- **PostHog analytics** (`cli/analytics/Analytics.kt`, `PostHogEvents.kt`):
  - Host `https://us.i.posthog.com`, with a hard-coded project key.
  - Events include `maestro_cli_command_run`, `test_run_started/failed/finished` and `workspace_run_started/failed/finished`, carrying flowCount, deviceCount, platform, durationMs, the error message and a sanitized `commandStringUsed`.
  - State lives in `$STATE/analytics.json`. Events are fire-and-forget on daemon threads, followed by `Analytics.flush()`/`close()`.
  - **Disabled by `MAESTRO_CLI_NO_ANALYTICS` set to any value** (presence check). **[V][D]**
- **Not covered by that switch [V]:**
  - `ErrorReporter.report()` POSTs the exception plus command-line args (with `-e` values hashed) to `$MAESTRO_API_URL` or `/maestro/error` on prod for *every* execution exception. It waits up to 1 s.
  - `ScreenReporter.reportMaxDepth()` runs in the shutdown hook and POSTs to `/maestro/screen`.
  - `Updates.fetchUpdatesAsync()` runs at every start and GETs `/v2/maestro/version`. `MAESTRO_DISABLE_UPDATE_CHECK=true` only suppresses *waiting for* and printing the result.
  - On air-gapped runners these calls fail quietly and add a small delay. Run Phase 0 offline to confirm nothing blocks or hangs.
- **Cloud vs local:**
  - `maestro cloud` uploads the app and workspace and polls. Its console reuses `TestSuiteStatusView` (`[Passed]/[Failed]`, Canceled/Timeout/Skipped variants).
  - Cloud adds `cloud.*` JUnit properties, `WARNING`/`CANCELED` statuses, flow-level smart retries, and recordings, logs and screenshots in the Maestro console.
  - The local CLI has none of these. Local `FlowStatus` is only `SUCCESS`/`ERROR`.
  - `--analyze` and AI commands send screenshots and logs to Maestro's API. **The agent should not enable them.**

---

## 12. Hook points that beat console scraping

Ranked by value divided by effort, for a **Rust** agent.

**H1. Configure and tail the session `maestro.log`. No Maestro changes. [V]**

- Set `MAESTRO_CLI_LOG_PATTERN_FILE` to a delimiter-safe, sortable pattern, for example:
  ```
  MFLT1|%d{UNIX_MILLIS}|%t|%level|%logger|%enc{%msg}{CRLF}%n
  ```
  `%enc{…}{CRLF}` escapes embedded newlines. `UNIX_MILLIS`, `%t` and `%enc{}{CRLF}` are standard log4j2 PatternLayout features **[K]**, so Phase 0 must verify them against the bundled log4j version.
- Tail the file with an inotify/kqueue reader (the `notify` crate) plus a byte-offset follower.
- Parse the plan, shard → device, flow start, step start/finish, retry attempts, failures and the system-info version.
- Thread name `%t` is probably stable per shard. Each shard runs inside `runBlocking` on its own `Dispatchers.IO` worker thread. **[I]**
- This is the closest thing to a structured live stream that exists today.
- **Risks:**
  - The message templates are not a contract, but they have been stable since at least v1.30.
  - Volume at root level `ALL` is unknown (Phase 0).
  - `%method` (default pattern) forces location capture. Omitting it from the custom pattern is cheaper.

**H2. Watch the filesystem. No Maestro changes. [V]**

Flow directory created means flow started. `screenshots/step-*` means a step failed or warned. `manifest.json` means the bundle is complete; the agent then parses `commands.json` for exact step data and timestamps. Use H2 to confirm and enrich H1.

**H3. Report from inside flows via hooks and `runScript` `http.post`. [V][D]** This needs repository cooperation.

- Each top-level flow (or a shared subflow it calls with `runFlow`) gets this hook pair:
  ```yaml
  onFlowStart:
    - runScript: fleet/start.js
  onFlowComplete:
    - runScript: fleet/end.js
  ```
  The scripts call `http.post(MAESTRO_FLEET_URL + '/flow', …)` and report `MAESTRO_FILENAME`, `MAESTRO_DEVICE_UDID`, `MAESTRO_SHARD_ID` and the run ID.
- **Drawbacks:**
  - There is no global hook, so every flow must be edited.
  - A JS `http` exception fails the flow (hook failure means flow failure), so scripts **must** wrap calls in `try/catch`.
  - The JS HTTP client has **5-minute** read/write/call timeouts (`maestro-client/src/main/java/maestro/js/GraalJsEngine.kt` L28-34). A hung agent endpoint would stall the test. The endpoint must answer instantly (localhost, non-blocking, 204).
  - Hook commands pollute step counts.
  - `onFlowComplete` does not report the flow's own pass or fail status to the script. **[I]**, since no status variable was found.
  - Every `MAESTRO_*` env value, a token for example, **is written to `commands.json`**. Pass only a non-secret localhost URL and run ID.
- Conclusion: **an opt-in enhancement, not the baseline.**

**H4. JVM agent or bytecode patching. Feasible but not recommended.**

- Inject `-javaagent:fleet-maestro.jar` via `JAVA_TOOL_OPTIONS`/`MAESTRO_OPTS`.
- With ByteBuddy, wrap `maestro.orchestra.Orchestra`'s constructor so it appends our `OrchestraListener`, and hook `TestSuiteInteractor.runFlow`.
- That would give exact typed events: `onFlowStart`, `onCommandStart(cmd, seq, depth)`, `onCommandFinished(outcome, startedAt, finishedAt)`, `onFlowEnd`.
- **Prior art:** DeviceLab's "maestro-complete-reports" "auto-detects Maestro, patches JARs" to intercept events, and was tested only on 2.0.9.
- **Costs:** a JVM artifact inside an otherwise all-Rust project, private-API coupling to classes that changed 11 times in 4 months, and a possible effect on test behaviour. Keep it as a fallback only if H1 proves inadequate.

**H5. Upstream a structured event reporter. Recommended to pursue in parallel.**

- **Architecture fit [V]:**
  - `OrchestraListener` (`maestro-orchestra/src/main/kotlin/maestro/orchestra/debug/OrchestraListener.kt`) already exposes every per-flow and per-command lifecycle event with sequence number, depth, epoch timestamps and outcome.
  - `Orchestra(listeners = …)` dispatches to listeners in isolation; a throwing listener is logged and cannot break the flow (`Orchestra.dispatch`, L1056-1067).
  - `TestSuiteInteractor` already builds `listOf(CliConsoleListener(shardPrefix))` (L194).
  - The suite-level facts (plan, shard → device, flow start/end with status) live in `TestCommand`/`TestSuiteInteractor`.
  - An SSE JSON event model already exists for the MCP viewer (`ViewerEvent`).
- **Proposed PR:** `maestro test --events <path|->` writes NDJSON with a versioned `$schema`, modelled on `manifest.json`'s `v1` approach. The events:
  - `suite_planned` {flows[], shards[{index, deviceId, flows[]}]}
  - `flow_started` {shard, deviceId, flowName, file, flowDir}
  - `command_started` / `command_finished` {seq, depth, description, type, status, startedAt, finishedAt, error{message}}
  - `flow_finished` {status, durationMs, failure}
  - `suite_finished` {passed, total}
- About 200-300 lines including tests. It is CONTRIBUTING "Type B" (features), which asks contributors to "open an issue or a PR with a suggestion first". Maintainers aim to review within a week and release about a week after merge.
- **Upstream signals:**
  - The project is very active: 185 commits since 2026-05, mostly by staff.
  - The 2.7 manifest work shows appetite for versioned machine-readable contracts.
  - I found **no existing issue requesting live or structured output**. Searches turned up only #1210 (steps in JUnit, open), #2963 (metadata in webhook payloads, closed) and #2512 (env secrets in `commands-*.json`, open).
- **Likely objection:** maintainers may prefer to keep events for Maestro Cloud and the Viewer. Frame the proposal as CI-observability parity with the MCP viewer stream.
- If it is merged, the agent prefers `--events` when `maestro --version` ≥ that release and falls back to H1 + H2 otherwise.

---

## 13. Progress-state classification (Maestro 2.12.1, local CLI, suite path, Android)

| Spec state | Classification | Best source (and fallback) | Evidence |
|---|---|---|---|
| **Planned flow executions** | **OBSERVABLE LIVE** (seconds after start), exact | `maestro.log` `Created execution plan: …` (all flows) and `[shard N] Selected device … with execution plan …` (per shard). Count only: stdout `Will split/run …` when sharded. For `--shard-all`, planned = N × flows. | `WorkspaceExecutionPlanner.kt` L179, `TestCommand.kt` L418-429, L475 [V] |
| **Started** (flow) | **OBSERVABLE LIVE** | `maestro.log` `Running flow <name>`. Flow directory creation. Not on stdout. | `TestSuiteInteractor.kt` L181-184 [V] |
| **Step started** | **OBSERVABLE LIVE via `maestro.log` only** (also stdout with `--verbose`) | `<desc> RUNNING`. Exact seq, depth and start time only afterwards, from `commands.json`. | `CliConsoleListener.kt` [V] |
| **Step completed** (incl. failed/skipped/warned step) | **OBSERVABLE LIVE via `maestro.log`**. Failure also via `screenshots/step-NNN-*.png`. **Exact data at flow end** in `commands.json`. | `<desc> COMPLETED\|FAILED\|SKIPPED\|WARNED` | `CliConsoleListener.kt`, `ArtifactsGenerator.kt` L140-174 [V] |
| **Completed** (flow) | **OBSERVABLE LIVE** | stdout `[Passed]/[Failed] <name> (<dur>)`. `manifest.json` written. | `TestSuiteStatusView.kt` L52-70, `ArtifactsGenerator.onFlowEnd` [V] |
| **Passed** | **OBSERVABLE LIVE** (provisional). Authoritative **only at end** (JUnit) | stdout `[Passed]`. JUnit `status="SUCCESS"`. | [V] |
| **Failed** | **OBSERVABLE LIVE** (provisional, with message). Authoritative at end. | stdout `[Failed] … (<msg>)`, `FAILED` log line, failure screenshot. JUnit `<failure>`. | [V] |
| **Skipped** (flow) | **INFERRED** | planned − executed. Only arises when an `executionOrder` sequence aborts (`continueOnFailure: false`); those flows are absent from stdout and JUnit. Tag-filtered flows are excluded from the plan, not "skipped". | `TestSuiteInteractor.kt` L82-89 [V] |
| **Skipped** (step) | **OBSERVABLE LIVE** (`maestro.log` `SKIPPED`) / at flow end (`commands.json`) | conditional `runFlow` `when:` false and similar | `Orchestra.kt` L390-394 [V] |
| **Interrupted** | **INFERRED** | process exit by signal (130/143/137) or crash, plus a flow directory without `manifest.json`, plus `Running flow` with no completion line. No JUnit is produced. | §8 [V]+[I] |
| **Unknown** | **INFERRED** | planned flows with no evidence after an abnormal exit | [I] |
| **Retried** (command-level) | **OBSERVABLE LIVE** (`maestro.log` `Attempt N failed for retry command`, repeated `RUNNING`/`PENDING`) / **AT END** (`commands.json` repeated entries) | not in JUnit | `Orchestra.kt` L995-1018 [V] |
| **Retried** (flow-level Attempt) | **UNAVAILABLE** from Maestro (it never retries flows locally). Agent-created only, when CI or the agent re-invokes. | — | [V][D] |
| **Device assignment** | **OBSERVABLE LIVE** | `maestro.log` `[shard N] Selected device <serial> using port <P>`. `~/.maestro/sessions` heartbeat keys. At end: the console shard box and JUnit `device=` (AVD name, not serial). Deterministic if `--device` is pinned: shard *k* = *k*-th ID. | `TestCommand.kt` L472-475, `SessionStore.kt` [V] |
| **Shard** (identity + per-flow shard) | **OBSERVABLE LIVE** | `[shard N] ` prefix on stdout and in `maestro.log`. Flow directory suffix `-shard-N`. `MAESTRO_SHARD_ID` inside flows. JUnit: suite order + `[shard N] ` failure prefix only. | `TestSuiteInteractor.kt` L49, `TestDebugReporter.kt` L103-114 [V] |
| Step timing (exact) | **AVAILABLE ONLY AT END OF EACH FLOW** | `commands.json` `timestamp`/`duration`/`sequenceNumber`/`depth`. Live log timestamps are millisecond-accurate approximations, `HH:mm:ss.SSS` by default; use `UNIX_MILLIS` via the pattern env. | [V] |
| Suite result / totals | **OBSERVABLE LIVE** at shard end (stdout summary). **AT END** (JUnit, exit code). | | [V] |

Without the `maestro.log` techniques (stdout only, as the spec assumes), *planned* (unless sharded), *flow started*, *step started/completed*, *device assignment* and *retry* all drop to INFERRED or UNAVAILABLE.

---

## 14. Recommendations for the Rust agent's live-signal strategy

1. **A canonical invocation, enforced or rewritten by the agent:**
   ```
   MAESTRO_CLI_NO_ANALYTICS=1 MAESTRO_DISABLE_UPDATE_CHECK=true MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true \
   MAESTRO_CLI_LOG_PATTERN_FILE='MFLT1|%d{UNIX_MILLIS}|%t|%level|%logger|%enc{%msg}{CRLF}%n' \
   maestro --no-ansi test --format junit --output $RUN/report.xml \
     --flatten-debug-output --debug-output $RUN/artifacts \
     [--device emulator-5554,emulator-5556 --shard-split 2] <flows>
   ```
   - `$RUN` is fresh per invocation.
   - Prefer `--flatten-debug-output` over `--test-output-dir`. With the spec's example, `maestro.log` and the bundles land in different subtrees, and the timestamped directory would have to be discovered.
   - **Always pin `--device`** when the agent knows the devices.
   - Keep the user's flags otherwise. If the user insists on `--test-output-dir`, glob for `<T>/*/` and `<D>/.maestro/tests/*/maestro.log` (§4.1).
2. **Version gate.**
   - Read `maestro --version` before the run (it costs one extra JVM start) or the `Maestro Version:` line from `maestro.log`.
   - Choose a parser profile: `≥2.7` uses the bundle layout; `<2.7` uses legacy `commands-(…).json` (best-effort only).
   - Mark unknown versions as "parsing degraded" instead of failing.
3. **Three evidence lanes, merged as provenance claims** (consistent with research 05):
   - (a) the `maestro.log` tailer, primary for plan, start, steps, device and shard;
   - (b) the stdout parser, primary for flow completion; tolerant regexes with a `[shard N] ` prefix and a `[Passed|Failed]` token;
   - (c) the filesystem watcher, which confirms bundles and parses `manifest.json` + `commands.json` at each flow end.
   - Final JUnit overrides provisional outcomes. Join on `testcase@file` + suite index (= shard), then by name.
4. **Never block the stdout reader.** Read stdout and stderr on dedicated tasks into bounded channels, and tee them to a local ring-buffered log file. Maestro's stdout must never back-pressure. Keep a full copy for `inspect`.
5. **Parse defensively:**
   - Match status keywords as suffixes, since descriptions and flow names are user text.
   - Treat the `ExecutionPlan(...)` `toString` as best-effort, since paths can contain `, ` or `]`. Cross-check counts against the per-shard lines.
   - Unknown lines are ignored and counted for a "parser drift" metric.
6. **Progress percentage:**
   - Exact only when the plan line was parsed, or a CI manifest is given, *and* the run isn't using `executionOrder` with `continueOnFailure:false`.
   - For `--shard-all`, the denominator is N × flows.
   - Otherwise show observed counts plus an indeterminate indicator, as the spec says.
7. **Interruption:**
   - Forward SIGTERM to the JVM PID. Grace period of about 10-15 s, configurable; the shutdown hook uninstalls APKs over adb. Then SIGKILL.
   - Exit with Maestro's code, or 128 + signal.
   - Mark flows that started but have no `manifest.json` as `interrupted` and planned-but-unstarted flows as `unknown`/`not_run`. Do not fabricate JUnit.
8. **Exit-code classification:**
   - `0` = passed.
   - `1` + report present = test failures.
   - `1` + no report = Maestro or infra error (keep the stderr tail).
   - `2` = usage error.
   - ≥128 = signal.
9. **Device side:** observe only, through the shared adb server. Stream `logcat` if needed; Maestro clears it per flow. Never use `uiautomator`, `kill-server` or `logcat -c`.
10. **Security:**
    - Never put secrets in `MAESTRO_*` env or `-e`. They land in `commands.json`, which the agent may upload.
    - Treat `commands.json` (`defineVariables` env, `evaluatedCommand` with interpolated text) and `logs/maestro.log` as **sensitive**. Apply redaction before upload, per spec §10.
11. **Optional H3** (hooks plus a localhost endpoint), as documented opt-in for teams that want flow-reported device and shard facts. **Pursue H5** (upstream `--events`) and switch to it once it ships.

---

## 15. What Phase 0 must verify against a real emulator

1. **Console samples on the suite path:** 1 pass, 1 fail, a multi-flow suite, `--shard-split 2`, `--shard-all 2`. Capture byte-exact stdout and stderr, with and without `--no-ansi` under a pipe. Confirm there is no flow-start line, the order of the `[Passed]`/summary lines, multi-line error text, and the duration formats.
2. **`maestro.log` liveness:** is it flushed per line, so a tailer sees `RUNNING` before `COMPLETED` in real time? What is the volume per step at the default level?
3. **`MAESTRO_CLI_LOG_PATTERN_FILE`:** do `%d{UNIX_MILLIS}`, `%t`, `%enc{%msg}{CRLF}` work with the bundled log4j? Is `%t` stable per shard?
4. **`--verbose`:** stdout volume, and whether `%highlight` emits ANSI under `--no-ansi`.
5. **Output paths for all six flag combinations in §4.1**, including the spec's `--debug-output X --test-output-dir X`. Settle the conflict between docs and code.
6. **Write timing:** the flow directory exists at start; `logs/maestro.log` grows; failure screenshot timing; `commands.json` before `manifest.json`; the console line after `manifest.json`; the report only at the end. Record a filesystem event trace (inotify).
7. **`commands.json` samples:** are `sequenceNumber: 0` / `depth: 0` emitted? Shape of `defineVariables` (secrets); hook commands; `retry` attempts; nested `runFlow`; `repeat`; optional `WARNED`; conditional `SKIPPED`.
8. **Per-flow `logs/maestro.log` cross-shard contamination** in sharded runs.
9. **JUnit samples:** single, sharded split and sharded all. `device=` value (AVD name?) for two emulators of the same AVD; suite order = shard order; failure text with `[shard N]`; non-Maestro exception stack in `<failure>`; time zone of `timestamp`.
10. **Device-correlation rule:** map `[shard N] Selected device <serial>` to JUnit suite N to the `-shard-N` flow directories to the `~/.maestro/sessions` keys. Test with `--device` pinned and unpinned (adb ordering).
11. **Exit codes:** pass, fail, no device, bad flag (expect 2), driver install failure, one shard's device killed mid-run (does any report appear?), SIGINT, SIGTERM, SIGKILL. What is left on disk in each case; whether the driver APKs are uninstalled on SIGTERM; how long the shutdown hook takes.
12. **Emulator crash mid-flow:** which error text and exception class; is the next flow attempted?
13. **Concurrent agent adb usage:** `dumpsys activity`, `getprop`, `dumpsys meminfo` every 5-15 s, and streamed `logcat`. Confirm no Maestro flakiness or measurable slowdown. Confirm that `uiautomator dump` does break Maestro, so we can document it.
14. **Process tree and overhead:** confirm `exec` (PID identity), the children listed in §9, start-up time spent in `avdmanager`/`flutter`, and the steady-state JVM RSS and CPU per shard.
15. **Offline runner:** with analytics disabled and no network, check start-up delay from the update check, error reporter and screen reporter.
16. **`executionOrder` with `continueOnFailure: false`:** confirm the skipped flows are absent from stdout and JUnit.
17. **Repeat Phase 0 on the CI's pinned Maestro version, if it is not 2.12.1,** and on the next minor. The parsers must survive at least N-1 to N+1.

---

## 16. Sources

### Maestro source at tag `v2.12.1` (https://github.com/mobile-dev-inc/maestro/tree/v2.12.1)

- `CHANGELOG.md`, `CONTRIBUTING.md`, `AGENTS.md`, `maestro-cli/gradle.properties`, `maestro-cli/build.gradle.kts`, `gradle/libs.versions.toml`
- `maestro-cli/src/main/java/maestro/cli/App.kt`, `DisableAnsiMixin.kt`, `ShowHelpMixin.kt`, `CliError.kt`
- `maestro-cli/src/main/java/maestro/cli/command/TestCommand.kt`
- `maestro-cli/src/main/java/maestro/cli/runner/TestSuiteInteractor.kt`, `TestRunner.kt`, `MaestroCommandRunner.kt`, `CliConsoleListener.kt`, `CommandState.kt`, `resultview/PlainTextResultView.kt`, `resultview/AnsiResultView.kt`, `resultview/UiState.kt`
- `maestro-cli/src/main/java/maestro/cli/view/TestSuiteStatusView.kt`, `view/ErrorViewUtils.kt`, `util/PrintUtils.kt`, `util/TimeUtils.kt`, `util/CiUtils.kt`, `util/EnvUtils.kt`, `util/IOSEnvUtils.kt`, `util/ErrorReporter.kt`, `util/ScreenReporter.kt`
- `maestro-cli/src/main/java/maestro/cli/report/TestDebugReporter.kt`, `JUnitTestSuiteReporter.kt`, `HtmlTestSuiteReporter.kt`, `ReportFormat.kt`, `ReporterFactory.kt`, `TestSuiteReporter.kt`
- `maestro-cli/src/main/java/maestro/cli/model/TestExecutionSummary.kt`, `model/FlowStatus.kt`
- `maestro-cli/src/main/java/maestro/cli/session/MaestroSessionManager.kt`, `session/SessionStore.kt`, `db/KeyValueStore.kt`, `device/PickDeviceInteractor.kt`
- `maestro-cli/src/main/java/maestro/cli/analytics/Analytics.kt`, `analytics/PostHogEvents.kt`, `update/Updates.kt`, `insights/TestAnalysisManager.kt`, `api/ApiClient.kt`
- `maestro-cli/src/main/java/maestro/cli/mcp/viewer/McpViewerEvents.kt`, `McpViewerServer.kt`
- `maestro-orchestra/src/main/java/maestro/orchestra/Orchestra.kt`
- `maestro-orchestra/src/main/java/maestro/orchestra/workspace/WorkspaceExecutionPlanner.kt`, `ExecutionOrderPlanner.kt`, `Filters.kt`
- `maestro-orchestra/src/main/kotlin/maestro/orchestra/debug/OrchestraListener.kt`, `ArtifactsGenerator.kt`, `BundleLayout.kt`, `TestOutputWriter.kt`, `FlowDebugOutput.kt`, `CommandStatus.kt`, `StepArtifactNaming.kt`, `DeviceArtifactCapturer.kt`
- `maestro-orchestra-models/src/main/java/maestro/orchestra/ArtifactManifest.kt`, `WorkspaceConfig.kt`, `MaestroCommand.kt`, `Commands.kt`, `util/Env.kt`; `maestro-orchestra-models/src/main/resources/maestro/orchestra/artifact-manifest/v1.schema.json`
- `maestro-client/src/main/java/maestro/debuglog/LogConfig.kt`, `ScopedLogCapture.kt`, `DebugLogStore.kt`
- `maestro-client/src/main/java/maestro/device/DeviceService.kt`, `android/AndroidDeviceConnection.kt`, `drivers/AndroidDriver.kt`, `js/GraalJsEngine.kt`, `Errors.kt`
- `maestro-utils/src/main/kotlin/Collections.kt`
- Tests: `maestro-cli/src/test/kotlin/maestro/cli/report/JUnitTestSuiteReporterTest.kt`, `TestDebugReporterTest.kt`, `cli/cloud/CloudInteractorTest.kt`; `maestro-orchestra/src/test/kotlin/maestro/orchestra/debug/ArtifactsGeneratorTest.kt`; `maestro-client/src/test/java/maestro/debuglog/ScopedLogCaptureTest.kt`
- E2E harness: `e2e/run_tests` (L87, L105, L123)
- History: `git log` on the files above; `git show v2.6.1:…/TestDebugReporter.kt` (legacy layout); `git show v1.30.0|v1.36.0|v1.39.0|v2.0.0:…` (template stability); commits `0d9b257e` (#3282 bundle), `a04388c0` (#3465 durations), `6d4b2ab9` (log pattern env), `436309bd` (#3693 dir-name shortening)

### dadb at tag `v2.0.0` (https://github.com/mobile-dev-inc/dadb/tree/v2.0.0)

- `dadb/src/main/kotlin/dadb/Dadb.kt` (`list`, L252-272); `dadb/src/main/kotlin/dadb/adbserver/AdbServer.kt` (`listDadbs`, L64-86); `dadb/src/main/kotlin/dadb/adbserver/AdbBinary.kt` (`ensureServerRunning`/`startServer`)

### Docs and web

- Test output directory: https://docs.maestro.dev/cli/test-output-directory
- Test suites and reports: https://docs.maestro.dev/cli/test-suites-and-reports
- CLI commands and options: https://docs.maestro.dev/maestro-cli/maestro-cli-commands-and-options.md
- Environment variables: https://docs.maestro.dev/maestro-cli/environment-variables.md
- Hooks: https://docs.maestro.dev/maestro-flows/flow-control-and-logic/hooks.md
- HTTP in JS: https://docs.maestro.dev/maestro-flows/javascript/make-http-requests.md
- Retry command: https://docs.maestro.dev/reference/commands-available/retry.md
- Maestro Cloud automatic retries: https://docs.maestro.dev/maestro-cloud/advanced-features/automatic-retries.md
- Docs index: https://docs.maestro.dev/llms.txt
- CHANGELOG: https://github.com/mobile-dev-inc/maestro/blob/main/CHANGELOG.md
- Issues search (JSON/report): https://github.com/mobile-dev-inc/maestro/issues?q=is%3Aissue+json+report (#1210, #2512, #2963, #2707, #1100)
- Issues search (progress/streaming/reporter): https://github.com/mobile-dev-inc/maestro/issues?q=is%3Aissue+%28progress+OR+streaming+OR+reporter+OR+listener+OR+%22real-time%22%29 (#3721 ANSI slowness on Windows, #3644)
- DeviceLab maestro-complete-reports (JAR-patching prior art): https://devicelab.dev/open-source/maestro-complete-reports
