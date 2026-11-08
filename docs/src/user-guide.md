# User guide

## Wrapping Maestro

Put `fleet-agent run --` in front of your `maestro` command:

```bash
fleet-agent run -- maestro test .maestro/
```

The agent never changes what CI sees: Maestro's output is passed through byte for byte, and the step's exit status is Maestro's own, signal deaths included. If Fleet is misconfigured or unreachable, Maestro runs exactly as it would without the agent.

What the agent adds to `maestro test`, only when you haven't set it yourself (turn off with `--fleet-rewrite=off`):

- `--flatten-debug-output --debug-output <run dir>/artifacts`, so the session log and flow bundles land in one place it can follow;
- `--format junit --output <run dir>/report.xml`, the final report it reconciles against, but only when that can't change how Maestro runs (see below);
- `MAESTRO_CLI_LOG_PATTERN_FILE`, a machine-readable log line, plus `MAESTRO_CLI_NO_ANALYTICS=1` and `MAESTRO_DISABLE_UPDATE_CHECK=true`.

It never removes a flag.

**Where the agent looks for Maestro's files.** While the run goes, the agent reads Maestro's session `maestro.log` and each flow's bundle (`commands.json`, `manifest.json`, screenshots) from the place Maestro writes them for the command as given. Maestro's rules (research 01 §4.1) decide that place, and the agent follows them for your flags, including when you set none of them and rewriting is off:

| Flags on `maestro test` | Session log | Flow bundles |
|---|---|---|
| none, with `--fleet-rewrite=off` | `$XDG_STATE_HOME/maestro/tests/<ts>/`, or `<home>/.maestro/tests/<ts>/` | the same directory |
| `--test-output-dir T` | the state directory, `<ts>/` | `T/<ts>/` |
| `--debug-output D` | `D/.maestro/tests/<ts>/` | the same directory |
| `--debug-output D --test-output-dir T` | `D/.maestro/tests/<ts>/` | `T/<ts>/` |
| `--flatten-debug-output --debug-output D` | `D/` | `D/` (`--test-output-dir` is ignored) |
| `--flatten-debug-output` alone, with `--fleet-rewrite=off` | `<home>/` | `<home>/` |
| `--flatten-debug-output --test-output-dir T` | `<home>/` | `<home>/` (`T` is ignored) |
| `--flatten-debug-output --debug-output D --test-output-dir T` | `D/` | `D/` (`T` is ignored) |
| `--config C` (or a `config.yaml` in a directory you pass), whose `testOutputDir: T` is set | the state directory, `<ts>/` | `T/<ts>/`, `T` relative to the working directory |
| `--config C --test-output-dir T2` | the state directory, `<ts>/` | `T2/<ts>/`: the flag wins over the config |

`<home>` is Maestro's `user.home`: the directory the JVM reports, which is your account's home unless `JAVA_OPTS` or `MAESTRO_OPTS` sets `-Duser.home`. Changing `$HOME` does not change it. `<ts>` is the `yyyy-MM-dd_HHmmss` directory Maestro creates for the run. The agent takes the one that its own Maestro process holds its `maestro.log` open in, so two runs in one tree never trade their steps. With no flags and rewriting on, the agent adds `--flatten-debug-output --debug-output <run dir>/artifacts`, so that row is the flattened one. Every row above is captured from Maestro 2.12.1 (`fixtures/maestro/2.12.1`, `paths-*`). A workspace config is read only from `--config` or from a `config.yaml` in a directory you pass, not from a single flow file beside it. A repeated `--flatten-debug-output` is a usage error in Maestro, so the agent never adds a second one. With `--flatten-debug-output` alone, Maestro writes into `<home>`: the agent looks there only for folders created since the run started, and it looks on a backoff, not on every poll.

**Why the JUnit report is conditional.** In Maestro, `--format` isn't just reporting: asking for a report makes `maestro test` take its multi-flow path, the one it uses for several flows, and that path rejects `-c`/`--continuous`. Fleet must not change how your command runs, so it adds the report only when Maestro would take that path anyway:

| Command | Report added? |
|---|---|
| `maestro test flows/` (a directory) | yes |
| `maestro test a.yaml b.yaml` (several flows), or a planner shard of several flows | yes |
| `maestro test --shard-all N …` with N > 1 | yes |
| `maestro test login.yaml` (one flow) | no |
| any command with `-c` or `--continuous` | no |
| any command with your own `--format` | no (yours is used) |
| a flow path that doesn't exist or can't be read | no |

Without the report, Fleet still follows the run from Maestro's session log, flow bundles and exit status; it just has no JUnit to reconcile against. Exit status 1 still counts as test failures when Fleet saw a flow fail in Maestro's output or flow bundles; if it saw none, the exit is classified as unknown (`other`) rather than as a Maestro error, which is reserved for a report that was expected and is missing. The agent logs its decision once, at info level (`junit report decision`).

## Configuration

| Variable | Meaning |
|---|---|
| `FLEET_SERVER` | Fleet server base URL (`https://…`). Unset: offline mode. |
| `FLEET_TOKEN` | API key for the project. |
| `FLEET_JOB_KEY` | Stable job identity. On GitHub use `${{ job.check_run_id }}`. Otherwise `GITHUB_JOB` plus a hash of `FLEET_MATRIX` is used. |
| `FLEET_RUN_DIR` | The agent's directory for this run (journal, spool, its own log). |
| `FLEET_GRACE` | Seconds between the first INT/TERM and killing Maestro's process group (default 15). |
| `FLEET_FINALIZE_TIMEOUT` | Seconds the agent may spend finishing after Maestro exits (default 60). |
| `FLEET_FLUSH_TIMEOUT` | Seconds the observer waits for the server to ack everything (default 20); capped at 3 seconds if the server never answered the WebSocket upgrade during the run. |
| `FLEET_UPLOAD_ARTIFACTS` | `off` stops the agent uploading evidence (failure screenshots, `commands.json`, the UI hierarchy of a step, the logcat excerpt of a failed flow, the JUnit report, `maestro.log`). On by default when `FLEET_SERVER` is set. |
| `FLEET_UPLOAD_TIMEOUT` | Seconds the agent waits at the end of the run for uploads to finish (default: half of `FLEET_FLUSH_TIMEOUT`). Unfinished uploads are abandoned. The journal is sent first, while uploads run; the run end is sent after the upload results, and the final flush gets what is left of `FLEET_FLUSH_TIMEOUT`. |
| `FLEET_UPLOAD_RATE` | Upload rate cap in bytes per second, shared by all uploads (default 4 MiB/s). |
| `FLEET_JOURNAL_MAX_BYTES` | Most bytes the local `journal.jsonl` may take (default 256 MiB, 268435456). Past the cap the file stops growing and ends in a `journal.truncated` line; the events still go to the server. Also `--journal-max-bytes` on `fleet-agent run`. See [Offline mode](#offline-mode-and-inspect). |

### What the agent uploads

At most two files at a time, hashed while they stream. Uploading never changes Maestro's result or output: a refused, failed or timed-out upload is recorded in the run's journal (`artifact.rejected`) and nothing else. **Text is redacted before it leaves the runner**: `commands.json`, the JUnit report, `maestro.log` and the logcat excerpt go through the same redaction as the console (`-e` values, `MAESTRO_*` values, secret-named variables, known token shapes). A UI hierarchy first has the value of its `text`, `accessibilityText`, `hintText` and `error` attributes replaced by `[REDACTED]`. A text that is not valid UTF-8, contains NUL bytes or is over 16 MiB cannot be redacted reliably, so it is **not uploaded**, and the run's journal says so (`artifact_not_uploaded`). Screenshots are binary: they are never redacted, only access-controlled on the server. Maestro's own `takeScreenshot` images and `startRecording` videos are not uploaded yet ([#133](https://github.com/moyaver/fleet/issues/133)).

`FLEET_REPOSITORY`, `FLEET_RUN_ID`, `FLEET_RUN_ATTEMPT`, `FLEET_SHA`, `FLEET_REF` and `FLEET_RUNNER` describe the CI run on systems other than GitHub Actions.

## Watching a run

Open the dashboard (your operator gives you the address; if they enabled single sign-on you are sent to your company login first, and back to the page you asked for). Everything updates by itself while a run is going: you never need to refresh, and the page says when it can't keep up (see [When data stops arriving](#when-data-stops-arriving)).

A run is followed in four screens, each a click from the one before. Every live page has one status line that a screen reader hears: a run starting or ending, a flow failing or being interrupted, a pipeline's verdict changing. Those events are read out in batches, at most once every five seconds; log lines and counts that move are never read out. On a phone (under 720 px wide) the top bar shows the brand mark and the live indicator; the pages, the project and the account are behind the ⋯ button.

**A. Overview** (`/`). The readouts count pipelines by state; the *Active emulators* tile, beside *Runner health*, counts emulators seen in the last two minutes whose adb state is online. *Active pipelines* lists every run in progress with its status, progress, passed and failed counts and elapsed time; *Runner health* shows each runner's host CPU, memory and temperature and how fresh they are; *Recently finished* keeps the runs that just ended. A run appears here as soon as its agent reports in. Click a pipeline to open it.

**B. Pipeline detail** (`/p/<project>/pipelines/<id>`). The header shows the verdict, derived from the pipeline's flows: *Failed* if any flow failed; *Passed* only when the plan's total is known and every flow is verified as passed; otherwise *No failures*. While its jobs are still running and no flow has failed, it reads *Running*. It also shows the commit and a link to the CI run. Below it: *Test progress*, *Failures* (each links to its flow), *Jobs and devices* (each job, its agent's liveness, its devices and suites, and every flow with its outcome and badge), the *Execution timeline* (one lane per device, with the same data as a table for keyboard and screen-reader users) and *Live activity*. A *Live* / *Reconnecting* indicator in the header says whether the page is receiving updates.

**C. Runner and device detail** (`/workers/<id>`, `/devices/<id>`). Charts of host, Maestro and device metrics, each labelled with what it measures and its unit (for example *Host CPU (%)*, *Maestro CPU (% of one core)*, *Host memory used (bytes)*). On a runner, Maestro and agent resource use is one chart per run, titled with the run's short ID, because runs on one runner share it; host readings are one series per runner, and the *Health now* card shows only host readings. On a device, *Device (adb and sensors)* shows adb state, boot completion, and guest CPU and available memory, sampled every 30 seconds. On an emulator, *Emulator CPU* and *Emulator memory* are the CPU and resident memory of its `qemu-system-*` process, sampled with the device readings. A device's AVD name and API level are kept with each sample, and after a flow fails on a device its resumed activity is recorded as one sample. A sensor the agent doesn't report, such as a thermal sensor on a machine without one, reads **unavailable (reason)**: Fleet draws no line and shows no number rather than inventing one. A device page also has a *Newest sample* card: the time the agent sampled the device, the shard it ran as, and every reading of that sample. A runner page has a *Newest host sample* card with every host reading of the machine's newest sample, each with its value or the reason it is unknown, such as *no pressure information (PSI), as on macOS*. A reading the agent did not take shows its **Missing** mark and the reason in words, such as *throttled: the host is under pressure* or *no emulator process maps to this device*.

**D. Flow investigation** (`/flows/<id>?project=<slug>`). The flow's outcome, Maestro's failure message, every *claim* about the outcome with its provenance badge (the live log's and the final report's, with the deciding one marked), and the steps as a tree: a step run by `runFlow` sits under the block that ran it, and a retried step shows its attempt (for example *attempt 2 of 3*). Evidence is described under [Investigating a failure](#investigating-a-failure).

### Provenance badges

Every outcome and reading says where it comes from, so you can tell a settled result from a guess. A settled result carries no badge of its own: each page or section says *Verified* or *Observed* once, in its heading, and a row is marked only when its value is an exception (*Provisional*, *Inferred*, *Missing* or *Stale*). The badges:

| Badge | Meaning |
|---|---|
| **Observed** | The agent saw it directly on the runner: a device's state, an exit code, a signal, a file. |
| **Provisional** | Read from Maestro's live output while the run is open. The final report may differ. |
| **Verified** | Confirmed by Maestro's final report. The result is settled. |
| **Derived** | Computed from other measurements (a rollup), not a raw reading. |
| **Inferred** | Fleet's explanation of something it did not see, such as a flow cut short by a signal. Never shown as passed. |
| **Missing** | Nothing was received for this value. A step the log started and never ended is Missing when the flow's final `commands.json` does not list it either. |
| **Stale** | No update within 25 seconds; the last known value, not a current one. |

While a run is live, outcomes come from Maestro's log and console (**Provisional**). When Maestro writes its report, the report decides (**Verified**). If Maestro never writes one, the outcomes stay as Fleet saw them, and a run that ends abnormally never shows as passed.

A group of values, such as a page's steps or a table's rows, says its provenance once, in its heading or caption. Settled values (**Observed**, **Verified**, **Derived**) carry no badge of their own. Only **Provisional**, **Inferred**, **Missing** and **Stale** values are marked on the value itself, with a glyph and a word.

Steps settle the same way, one row at a time. A step's row is **Provisional** while it is read from the log, and only such a row carries its own badge; a settled row carries none. When the flow ends with a final `commands.json`, each command is paired with the step the log reported at the same nesting depth and of the same kind (a tap, an assertion, a launch, and so on). A paired row takes the file's result and, from the file, its start and duration, and becomes **Verified**. Pairing never crosses a depth or kind mismatch, and a step whose kind Fleet does not know is never paired. When the log and the file list a different number of steps, the steps that do line up still settle, and the rest stay **Provisional** (a partial match). The Steps section shows the weakest of its rows, so one row still read from the log keeps the whole section Provisional. Above the list, the section says when some steps could not be matched to `commands.json`, or when the log and the file disagree on a step's status; in that case the file's status is shown. A flow cut short, or whose `commands.json` is empty, aborted or has a status Fleet does not recognise, settles none of its steps: they stay Provisional. While a running flow's agent has gone quiet (stale, or disconnected), its Provisional rows show Stale, since their values are the last ones known. When the server abandons the run, its open steps show Inferred and read Unknown: they were never observed to end, and they are never shown as passed. A `commands.json` whose flow the log never started (Maestro's default console format names no flow, for example) creates no flow and settles nothing; the job's entry on the pipeline page says so.

### Progress, and when it is uncertain

*Test progress* reads **"N of M flows finished"** when Fleet knows the total: when a [Fleet plan](https://github.com/moyaver/fleet/blob/main/docs/decisions/0013-fleet-planner.md) was uploaded for the CI run, M is the plan's flows across all shards, known before the first job starts. M never changes while flows finish (unless more flows run than were planned, which raises it), and N never goes down.

Without a plan Fleet can't know how many flows other jobs of the pipeline will still run, so it does not guess: the page counts what finished and says **"progress uncertain (no plan)"**, with no bar and no percentage. A single suite can still count its own flows ("4 of 4 flows finished") when Maestro's log said which flows it would run.

### When data stops arriving

| What you see | What it means |
|---|---|
| **Online** | The agent is connected and has been heard from recently. |
| **Stale** | The agent is connected but silent for more than `liveness.stale_after_ms` (25 s by default). Its badges turn **Stale**: what you see is the last known state, not a live one. The same happens to every active run while the server restarts, until its agent reconnects. |
| **Disconnected** | The agent's connection closed without saying goodbye (a network cut, a killed runner). The agent keeps its events and sends them when it reconnects, so a run that comes back picks up exactly where it was. |
| **Completed** | The agent finished and said so. |
| *Reconnecting* (page header) | The browser lost its connection to Fleet. The page keeps showing the last state it had, says so, and takes a fresh snapshot as soon as it reconnects. |

A run that stays disconnected longer than `liveness.resume_grace_ms` (15 minutes by default) is abandoned: its unfinished flows become *unknown* (inferred by the server, never passed) and its jobs *interrupted*.

The server also limits how often each viewer credential may read (`429 Too Many Requests` with `Retry-After`). The dashboard waits that long and keeps the data it has on screen.

## Reading results: provenance

See [Provenance badges](#provenance-badges) above. The agent adds the same information to its own output: `fleet-agent inspect` prints each flow's outcome with its source, and a failure caused by a lost device (the same two signs as the dashboard's label) is prefixed "infrastructure (device lost)".

`GET /api/v1/flows/{id}/page` redirects a signed-in reader to the flow's page (`/flows/{id}?project=<slug>`) when only the flow's ID is known. A browser without a session is sent to sign in (`/api/v1/auth/login?return_to=…`) and comes back to the flow; an API client gets `401`. The job summary links each flow that did not pass through it, because the agent does not know the project's slug. A caller who may not read the flow's project gets the same 404 as for a flow that does not exist.

## Investigating a failure

A failed flow's page (open it from the pipeline page, from the pipeline's list of failures, or from a flow's earlier executions) is built to answer "why did this fail?" without the CI logs, and without the runner: once the run has ended, everything below is read from Fleet's own database and blob store, and nothing contacts the machine that ran Maestro.

- **Failure** names Maestro's message, the step that failed and the last step that succeeded. **Steps** lists every step with its result, duration and provenance, nested under the block that ran it. When the device was lost during the flow, the failure is labelled **Failed · infrastructure (device lost)**. The label needs two things the agent recorded: the failure names Maestro's `DeviceServerDiedException`, and the flow's `commands.json` was cut short (the agent's `commands_not_final` warning). The label names both as its evidence, carries the provenance of the failure it sits on (a verified failure shows no mark of its own), and says the failure is excluded from flakiness. The flow still reads **Failed**: this is a label, and the outcome and the CI exit code are unchanged. A failure that names the exception without the cut-short warning is not labelled.
- **Evidence** is what the agent uploaded for the flow:
  - **Failure screenshot**: the screenshot Maestro took at the failing step, with zoom (`+`, `-`, `0`) and a strip when there are several.
  - **Maestro log**: Maestro's session log for the whole run (redacted on the runner), opened at the failing step of *this* flow. Pick another step in the step tree and the log moves to where that step started; **Jump to the failing step** brings it back. The log has search (Enter and Shift+Enter step through the matches). The log is shown as Maestro wrote it (`MFLT1|time|thread|level|logger|message`); a log with no step markers for this flow is still shown, just without jumps. A log over 8 MiB is offered as a download instead.
  - **Command timings**: one row per command from `commands.json`, in order, with its result and duration; commands nested inside `runFlow` are indented.
  - **UI hierarchy**: the view hierarchy Maestro wrote for a step, listed under that step, with the failing step first. On-screen text is replaced by `[REDACTED]`; the tree, classes, bounds and resource IDs are kept.
  - **Device logcat excerpt**: the device log from 10 seconds before the failing step started to 2 seconds after it ended, redacted, at most 512 KiB. When it is cut, the newest lines are kept and the file says how many older lines were left out.
  - **App crash**: if the app died during the flow, the page says so above Maestro's failure, as "App crashed: <exception> in <package>", with a link to the device logcat excerpt (or a note that none was uploaded). It is evidence, not a verdict: the flow's result and the failure message below it stay Maestro's own.
  - **Maestro HTML report**, when one was uploaded, opens in a new tab. **Files** lists the other uploads (`commands.json`, the JUnit report, the log) to save.
- **Where the files come from.** Screenshots, logs and reports are served from the **usercontent domain** (`usercontent.url`, see the [operator guide](operator-guide.md#usercontent-domain)), never from the dashboard's own address, because a report is a file from a CI job: opened there it gets no cookie, no access to the dashboard and no scripts (`Content-Security-Policy: sandbox`). The dashboard asks for a short-lived signed link for each file; the page renews them before they lapse (5 minutes by default), and a link copied out of the page stops working after that. If you leave a page open for a long time and a download answers "this link has expired", reload the page.
- **When there is no evidence** the page says why instead of showing an empty box:
  - *No evidence was uploaded for this flow*: the run came from a fork pull request, the agent had no Fleet credential or `FLEET_UPLOAD_ARTIFACTS=off`, or the flow left nothing to upload (screenshots are only taken when a flow fails).
  - *The evidence is stored, but this server has no usercontent origin configured*: ask your operator to set `usercontent.url`.
  - *Refused uploads* lists a file the server verified and refused (too large, not the declared type, an unsafe archive, …) with the reason. Nothing else about a refused file is kept.

## History and compare

**History** (`/history?project=<slug>`) lists finished flow executions, newest first, within a window of at most 90 days (30 by default). Filter by flow (text in its path or name), branch, outcome and time; the filters live in the address, so a reload or a shared link shows the same table. Every row carries its provenance, its failure fingerprint (failures with the same fingerprint are the same failure) and the first digits of its condition fingerprint (runs with the same one ran under comparable conditions). A failed row whose device was lost during the run also shows **Failed · infrastructure (device lost)**, with the provenance of its failure when that is not verified: the row still reads Failed, and the run is left out of flakiness. A flow is marked **Flaky (confirmed)** when it passed and failed on one commit under one condition, and **Suspected flaky** when at least five eligible default-branch runs share a condition and at least 20% of adjacent run pairs switch between pass and fail in the latest 20 eligible runs. Both verdicts link to the runs that support them. An inferred outcome never counts as a pass. *Stats* on a row opens the flow's panel: counts of settled passes and failures, the median and 90th-percentile duration of its passes, a duration chart and its recent failures grouped by fingerprint.

**Compare** (`/compare?project=<slug>&a=<attempt>&b=<attempt>`) sets two attempts side by side: newly failing, recovered, changed, only in A, only in B and unchanged flows. A difference is explained only from what the evidence says: the conditions differ (and in which facts), the two failed differently, the flow is missing on one side, or one outcome rests on weaker evidence (provisional, inferred). A duration change is shown, never called a regression, and carries a note when the conditions differ. When the evidence says nothing differs, the row has no explanation.

**Failure triage** keeps a known failure from being investigated twice. Each failure shows *First seen* with the date and time its fingerprint was first seen. Each failure fingerprint is *untriaged* until a maintainer (or an admin) marks it **Known**, with the link to the issue that tracks it, or **Resolved**, optionally with the link to the change that fixed it; choosing *Untriaged* again clears it. The state shows on the history rows, on each group in a flow's stats panel and on the failure band of the flow page, so every failure with that fingerprint carries it. Fleet records when each fingerprint was first seen, and every change of state writes an audit event naming who made it and the state it changed from and to. A viewer sees the state and the link, but no controls.

The data comes from `GET /api/v1/projects/{project}/history`, `…/flows/{name}/stats` and `…/compare?a=&b=`; see `/api/v1/openapi.json`. Triage is `PUT /api/v1/projects/{project}/failures/{fingerprint}/triage` (body `{"state": "known" | "resolved", "issue_url": "https://…"}`, the link required for `known`) and `DELETE` on the same path, from a signed-in maintainer's browser session with its CSRF token.

## Offline mode and `inspect`

Without `FLEET_SERVER`, everything still happens locally: the run directory gets `journal.jsonl` (every event) and `spool.bin` (capped at 64 MiB; if a run overflows it, the spool ends in a `journal.gap` saying which events it does not hold). `journal.jsonl` is capped at 256 MiB (`FLEET_JOURNAL_MAX_BYTES`). When a run reaches the cap, the file stops with one `journal.truncated` line that gives the first event it does not hold and the bytes it wrote. The events still go to the server, the run's result is unchanged, and the end of the run prints one warning saying which `seq` the journal stops before. To see what Fleet makes of a run:

```bash
fleet-agent inspect "$FLEET_RUN_DIR"
```

This reads `journal.jsonl`, and says when it is truncated and which events are missing. Pointed at a directory with Maestro's saved log (`…/artifacts`), `inspect` replays that log instead, and when the same directory also has a `journal.jsonl` it reports that journal's state too.

`--json` prints the result as JSON; `--json --events` adds every journal event.

A run with no server also writes `fleet-bundle.tar.zst` into its run directory: its journal and the artifacts it would have uploaded, redacted. `fleet-agent publish` sends it later; see [Offline runs and `publish`](runner-guide.md#offline-runs-and-publish).

## When the agent dies: `flush`

If the runner kills the agent before everything reached the server, an `if: always()` step sends the spool:

```bash
fleet-agent flush "$FLEET_RUN_DIR/spool.bin"
```

The server ignores events it already has, so flushing twice is safe. On ephemeral CI runners, add `fleet-agent flush` in an `if: always()` cleanup step before the runner is discarded; the local spool is otherwise lost.

## Fleet Planner shards

A [Fleet plan](https://github.com/moyaver/fleet/blob/main/docs/decisions/0013-fleet-planner.md) splits the suite over CI jobs by duration. Run the planner from the repository root in the planning job; it writes the manifest and a matrix with one entry per shard, and publishes the plan to Fleet when it can (a Fleet outage only costs the planned totals, never the run):

```bash
fleet-planner plan --config .maestro/config.yaml --shards 4 \
  --manifest plan.json --matrix matrix.json \
  --server https://fleet.example.com --token "$FLEET_TOKEN" \
  --history-server https://fleet.example.com --project my-app
```

Each test job runs one shard. The shard's flows are the only flows the command runs, so don't name any flow paths yourself:

```bash
fleet-agent run --plan plan.json --shard 2 -- maestro test
```

The agent passes the plan's workspace config to Maestro, so `executionOrder` and tag rules keep their meaning. A plan that can't be read stops the step (exit 2) rather than running the whole suite on every shard. So does a shard with no flows, a command that isn't `maestro test`, and a command that names flow files or directories (`maestro test .maestro/`), selects tags with `--include-tags` or `--exclude-tags`, or passes its own `--config` when the plan has one. Maestro options such as `--device` and `-e` are fine.

The pipeline page shows each shard's planned time against its actual time (first flow start to last flow end), and the slowest shard against an equal-count split of the same flows by the same estimates. The plan manifest records that baseline; plans published before it existed show no comparison.

## Example workflow

See [`examples/github-actions/fleet-agent.yml`](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent.yml).
