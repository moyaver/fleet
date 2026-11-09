# 13: Coordinate mode (dynamic flow scheduling): measured

**Research date:** 2026-11-09
**Issue:** [#161](https://github.com/moyaver/fleet/issues/161)
**Decision:** [ADR-0015](../docs/decisions/0015-coordinate-mode.md): not built. Rejected under the owner's 20% rule (§7).
**Scope:** whether Fleet should hand Maestro flows to whichever CI worker is free, measured against equal-count sharding and against Fleet's static planner. Assets and raw results: [`13-assets/`](13-assets/README.md).
**Builds on:** [08 Phase 0 results](08-phase0-results.md) (evidence style, Maestro console and log facts), [11 Marathon and Flank](11-marathon-and-flank.md), [12 Positioning](12-positioning-and-borrowing.md), [ADR-0013](../docs/decisions/0013-fleet-planner.md), [ADR-0002](../docs/decisions/0002-agent-supervisor-observer.md).

## Owner decision (set before measuring, 2026-11-09)

**Threshold X = 20%.** Coordinate mode is worth building only if a dynamic prototype gives at least 20% lower p50 pipeline time than Fleet's static planner on a suite with real duration variance, with no correctness regressions.

The threshold was written into the brief before any run. It was not changed afterwards.

## How to read this report

- **[V]**: measured here, on the host and date given. Raw data is in `13-assets/results/`.
- **[V, fetch]**: read through a page-summarising fetch on 2026-11-09. Checked against the raw repo file where one exists.
- **[I]**: inferred or not measured. Treat as a hypothesis.

Method and the suite are described in §1. Anything that is synthetic is said so.

---

## 1. Method

### 1.1 Setup [V]

| | |
|---|---|
| Host | macOS 26 (Darwin 25.6.0), Apple M1 Pro, 16 GiB. Host load during the runs was high: 1-minute load average 14 to 15, and about 4.2 million pages in the compressor. Other agents were building and running emulators (see §8). |
| Maestro | 2.12.1 (`/opt/homebrew/bin/maestro`), the same version as research 08. |
| Emulators | Two throwaway AVDs, `fleet161a` (serial `emulator-5610`) and `fleet161b` (serial `emulator-5612`). Image `system-images;android-33;google_atd;arm64-v8a`. Headless, `-no-snapshot`, `-gpu swiftshader_indirect`, 2 GB RAM. AVD home was a `mktemp -d` directory, deleted at the end (§9). |
| App | `com.fleet.anr`, the ANR test app from `fixtures/android/anr-app` on branch `wave/4` (commit `d2ffd26`). It is not on `main`. Built with its own `build.sh` (SDK only, no Gradle, no downloads). Installed on both emulators. |
| Why that app | The ATD image has no Settings app. Its only launchable apps are `com.android.stk` and DocumentsUI. Research 08 used Settings on a Google Play image, which this image cannot reproduce. |
| Maestro flags | `--config suite/config.yaml --format junit --output <file> --debug-output <dir>`, with `MAESTRO_CLI_NO_ANALYTICS=1` and `MAESTRO_DISABLE_UPDATE_CHECK=true`. No retries are configured in any mode. |

### 1.2 Suite [V, synthetic variance]

Twelve flows in `13-assets/suite/`. Each flow clears app state, then repeats `stopApp`, `launchApp`, `assertVisible "Crash app"` *n* times. The repeat counts are 1, 2, 3, 4, 6, 8, 10, 12, 14, 16, 18 for `f01` to `f11`. `f12` waits 3 s for text that never appears, so it fails. Expected outcomes: eleven `passed`, one `failed`.

**The variance is designed, not observed.** Flow time runs from 1.9 s (`f01`) to 17.9 s (`f11`), a ratio of about 9.6. The brief asked for real duration variance. This suite has the variance I chose, not the variance of a real customer suite. §6 says what the repo does and does not show about real suites.

### 1.3 Modes [V]

All modes use the same two devices, the same flows and the same flags. Each run ends with `result.json`: pipeline wall time, device occupancy, flow time per device, every attempt of every flow, kills, and a correctness block.

| Mode | What runs |
|---|---|
| `shard-split` | One process: `maestro --device A,B test --shard-split 2 <suite>`. This is equal-count CI-style sharding. |
| `static` | Fleet's planner: `fleet-planner plan --shards 2`, then one `maestro test` process per shard, one shard per device, both at once. |
| `dynamic` | The prototype (`bench.py dynamic`). Flows are sorted longest first from the estimates file. A queue hands each flow to whichever device is free, one `maestro test` process per flow. |
| `dynamic-batch3` | The same prototype, but each item is three consecutive flows in one process. |
| `serial` | One process per flow on one device, back to back. Used for calibration and for per-invocation overhead. |
| `single` | One process over the whole suite on one device. The other side of the per-invocation overhead comparison. |

**Fleet history.** No Fleet server was running, so the planner had no history. Instead, the estimates are the median flow time from three `serial` runs (`results/cal-serial-{1,2,3}`), rounded to whole seconds because the planner's estimates file is `u64`. This stands in for Fleet's per-flow p50. It is not Fleet's history store, and it is three samples, not a real p50 over CI history.

**Planner output** (`results/plan-2.json`, plan `plan-3d7a1164…`): shard 0 holds `f11, f08, f07, f04, f03, f02` (estimate 52 s); shard 1 holds `f10, f09, f06, f05, f12, f01` (estimate 51 s). The planner's own equal-count estimate for the same units is 54 s.

**Repetitions.** Three runs per mode, with the mode order rotated between repetitions. The brief asked for at least three. Because the host was loaded, the spread is reported with the p50.

### 1.4 Correctness check [V]

For every flow: the number of completed attempts, the final outcome, whether the flow was reported more than once by a process, and whether the final outcome matches the expected one. Outcomes come from two sources, as in Fleet's claim lattice (`crates/fleet-core/src/claims.rs`): the console line `[Passed] <flow> (<dur>)` (provisional, printed as it happens) and the JUnit report (final, written only at the end of a process). A killed process leaves no JUnit, so its console lines are all there is.

---

## 2. Raw numbers

Pipeline time is wall-clock from the first process spawned to the last one exiting. All times are seconds.

### 2.1 Three-way comparison (three reps each) [V]

| Mode | Rep 1 | Rep 2 | Rep 3 | **p50** | Spread (max − min) | Idle device time p50 (sum of both devices) | Correct in every rep |
|---|---|---|---|---|---|---|---|
| `shard-split` (equal-count, one process) | 68.3 | 69.0 | 65.8 | **68.3** | 3.2 | 0.0 | yes |
| `static` (fleet-planner, two processes) | 65.1 | 72.4 | 99.2 | **72.4** | 34.1 | 1.1 | yes |
| `dynamic` (prototype, one process per flow) | 134.0 | 128.2 | 139.8 | **134.0** | 11.5 | 0.4 | yes |
| `dynamic-batch3` (prototype, three flows per process) | 78.4 | 89.3 | 101.6 | **89.3** | 23.3 | 9.9 | yes |

Rep 1 of `dynamic` was run twice. The first run (101.5 s) used an earlier version of the prototype that did not yet have batching. Its result directory was overwritten by the rerun (134.0 s), so the 101.5 s figure survives only in the terminal log. The rerun is the one in the table. The earlier number is a sign of how much the per-flow mode moves between runs.

### 2.2 Per-invocation overhead (one device) [V]

| Mode | Rep 1 | Rep 2 | Rep 3 | **p50** |
|---|---|---|---|---|
| `single`: one `maestro test` over all 12 flows | 113.5 | 114.4 | 117.0 | **114.4** |
| `serial`: 12 `maestro test` processes, back to back | 173.6 | 181.8 | 189.6 | **181.8** |

The difference is (181.8 − 114.4) / 11 ≈ **6.1 s per extra invocation**. A separate count from the first `serial` run gives the same order: each process's wall time minus its JUnit flow time averages **6.6 s**. That covers JVM start, driver connect, app launch and `clearState`. This matches the brief's expectation that the overhead is large against these flow lengths.

### 2.3 Kill test: one device's Maestro process SIGKILLed 30 s into the run (one rep each) [V]

| Mode | Pipeline (s) | What happened |
|---|---|---|
| `static` | 75.7 | Shard 0 on `emulator-5610` was killed while running `f11`. Six flows (`f02, f03, f04, f07, f08, f11`) never produced a verdict. Nothing re-ran them. Shard 1 finished. **Six of 12 flows lost.** |
| `dynamic` (per flow) | 128.2 | The killed attempt (`f11`) was re-queued once, as designed, and ran again. The run finished with no flow lost, but **three flows that should pass reported `failed`** (`f07`, `f08`, `f11`), each in about 0.1 s, on `emulator-5610` straight after the kill. See §2.4. |

### 2.4 What broke after the kill [V]

The three false failures all came from `emulator-5610`, the device whose Maestro process had been killed. The log for each one (`results/kill-dynamic/logs/b00{3,5,6}.emulator-5610.log`) says:

```
maestro.android.DeviceServerDiedException: Device server died during 'deviceInfo' on emulator-5610 …
Caused by: io.grpc.StatusRuntimeException: UNAVAILABLE
Caused by: java.io.IOException: Command failed (tcp:65090): closed
```

The next Maestro process on that device could not reach the on-device driver that the killed process had been using. The next three attempts on that device (`f11`, `f08`, `f07`) all failed in the same way, so the device was unusable for several flows in a row. Later, a plain single-flow run on the same device passed (rc 0), so it recovered. The exact recovery condition was not traced. The point stands: **a killed Maestro process can leave the device in a state that turns passing flows into failures**. Nothing in the prototype resets the device. The prototype also cannot tell an infrastructure failure from a test failure, so it records these as real verdicts.

This is a false red, not a false green. Invariant I3 is about false passes, so this does not break I3 as written. It is still a correctness regression for the prototype, and the owner's threshold forbids that.

### 2.5 Correctness, all non-kill runs [V]

Every non-kill run (shard-split, static, dynamic, dynamic-batch3, single, serial) had each flow reported exactly once and every outcome matched the expected one (`all_correct: true`). `f12` failed as expected in every run.

---

## 3. Derived metrics [V]

- **Dynamic prototype vs static planner, p50.** `dynamic`: 134.0 vs 72.4, which is 85% **slower**. `dynamic-batch3`: 89.3 vs 72.4, which is 23% **slower**. Neither is 20% faster.
- **Threshold.** To meet X = 20%, a coordinate mode had to reach a p50 of at most 0.8 × 72.4 = **57.9 s**. The best prototype reached 89.3 s.
- **Planner vs equal-count, p50.** `static` 72.4 vs `shard-split` 68.3: the planner was **not** faster on this suite. Its spread (34.1 s) is wider than the difference, so the two cannot be separated with three reps.
- **Where the time went.** In the `dynamic` runs each device ran six `maestro test` processes. The median overhead per process was **9.5 s** (6.5 s when one device ran alone, see §2.2), so about 57 s per device went to invocation overhead. Each device also spent about 71 s in flow time, which adds to about 131 s, matching the 134 s pipeline. Flow time per device is larger than the serial run's total of 102 s, because two emulators running at once on a loaded host slow each other down: total flow time rose from 102 s (one device, serial) to about 141 s (two devices, dynamic). `dynamic-batch3` cuts the number of processes per device to two, which brings the figure to 89 s. It does not reach the static run's 72 s, because the batches are uneven and the last batch on one device idles while the other finishes (9.9 s total idle).
- **Idle device time.** The static planner's idle time is small (1.1 s p50 in total), because the two shards are close in length. The static planner has little idle time to recover on this suite. The dynamic queue's idle time is smaller still, but it costs more in overhead.

---

## 4. Invariants (written answers)

Each answer below is what the design would need, given the measurements. The decision is to reject coordinate mode, so none of these answers is being implemented.

### I1: pass-through [I, from the design]

In coordinate mode the CI job still runs Maestro, so Maestro's exit code is still the job's exit code. The question is what a worker reports when the whole run finishes. A worker would not be able to exit 0 unless every flow it was given had a final verdict. Under the kill test, the static run exited with a missing shard, so Maestro's exit code alone would have passed a run that lost six flows. A coordinate mode would have to check that every planned flow has a final verdict before it passes the job. A missing verdict must fail the job, not pass it.

### I2: fail open [I, from the design and §2.3]

The server would be on the critical path, because workers pull work from it. The fallback must be the static plan, then plain `--shard-split`, selected at job start if the server is unreachable. A mid-run outage must not stop a worker's current Maestro process. Already-leased flows would need a lease expiry, and a flow whose lease expired without a verdict may be re-run once (§2.3). Nothing in the measurements shows that this reclaim is safe without the device reset that §2.4 shows is missing.

### I3: no false pass [I, from the design and §2.4]

A retried-then-passed flow must be labelled flaky, not passed. Marathon Cloud does label fail-then-pass as flaky, and it still counts that as passing for the exit code (§5.2). Fleet's claim lattice would need a `flaky` outcome in the same style. Retries should be allowed only for infrastructure failures (a killed process, a device that dropped, a timeout), with a per-run quota. §2.4 shows that a device error is not always detectable from the process exit code, so the classifier would need the `DeviceServerDiedException` signature, which is the kind of Maestro-version-specific parsing research 08 warns about. Test failures are never retried.

### I10 and ADR-0002: the agent stays light [I]

A worker mode would add lease heartbeats and a work client to the agent. The agent's supervisor/observer split (ADR-0002) keeps the supervisor on std only, so a lease client belongs in the observer, not in the supervisor. Whether it belongs in `fleet-agent` or a separate binary is a question the owner would have to answer. A separate binary would keep the agent's overhead budget (I10, under 1% CPU) untouched when coordinate mode is off.

---

## 5. Execution correctness (points at the existing code)

| Requirement | Where it stands today [V] | What a coordinate mode would add [I] |
|---|---|---|
| Durable leases with expiry | None. There is no work assignment in Fleet, so there is nothing to lease. | A lease table keyed by plan ID and flow, with an expiry. |
| Heartbeats and worker loss | The agent heartbeat (`crates/fleet-server/src/config.rs:559`, `heartbeat_interval_ms`) is liveness for telemetry, not a lease. | A worker heartbeat that renews leases. Worker loss is a lease that expires. |
| Duplicates and idempotent ingest | The claim lattice is a pure function (`crates/fleet-core/src/claims.rs`): any delivery order and duplicate gives the same outcome. Journal rows use `ON CONFLICT DO NOTHING` (`crates/fleet-store/src/ingest.rs:1006`), and worker identity is an upsert (`ingest.rs:826`). | A duplicate execution would be two flow executions for one planned flow. The claim lattice already resolves the verdict, but the work-assignment side would need the same idempotency. |
| Cancellation | A CI cancel (SIGINT, then SIGTERM, then kill) is handled by the agent (ADR-0002). A superseded run and a server restart are not handled. | A cancel must release the leases of the cancelled run. |
| `executionOrder` units | Planner keeps each `executionOrder` sequence in one shard (`crates/fleet-core/src/plan.rs`). Maestro's `continueOnFailure: false` skips later flows (research 08, Q16). | A dynamic queue would need the sequence as one unit. This was not tested, because the suite has no `executionOrder`. |
| Mutually exclusive groups | Declared in planner constraints (ADR-0013). | Two workers must never run a group at once. Not tested. |
| Shared state (accounts, backend) | Not modelled. | Not tested. |
| Per-device limits | One process per device in every mode measured. | Not tested beyond one process per device. |

---

## 6. Is the suite realistic? What is and is not real-suite evidence

- **Real, in this repo:** research 08 captured Maestro 2.12.1 runs of four tiny Settings flows (`fixtures/maestro/2.12.1/suite/stdout.bin`). Their console durations were 10 s, 10 s, 12 s and 19 s (the 19 s one fails). That is real variance, but on flows of about 10 s, and on a different image.
- **Real, but not in the repo:** research 08 also analysed about 550 sweeps and 532 local runs of a private app on Maestro 2.11.0. Its findings are summarised there, and the data is not in the repo. It cannot be cited here as evidence for variance.
- **Not pulled:** Fleet's own CI history. `gh run list` on `moyaver/fleet` worked, but its workflows are not Maestro customer suites, and I did not pull per-flow durations from them.
- **Synthetic:** the 12-flow suite in §1.2. Its variance is designed. It shows how each mode behaves when variance is large. It does not show how often variance is large in real suites.

So the measurements answer "how do the modes behave with per-invocation overhead of about 6 s and flows from 2 s to 18 s". They do **not** answer "how much variance is in a customer's suite". Fleet has no evidence for that in the repo. That is the gap a future study would have to close, and it is one of the reopen conditions in ADR-0015.

---

## 7. Decision

Rule (§ owner decision): coordinate mode is worth building only if the dynamic prototype's p50 is at least 20% below the static planner's, with no correctness regressions.

- p50 static 72.4 s; threshold 57.9 s.
- Best dynamic prototype: 89.3 s (`dynamic-batch3`). Per-flow: 134.0 s.
- Correctness: a killed worker left three passing flows reported as failures (§2.4).

Both conditions fail. **Coordinate mode is rejected**, and observe mode plus the static planner stay as they are. [ADR-0015](../docs/decisions/0015-coordinate-mode.md) records this, with the reopen conditions.

---

## 8. Not verified

- Real-suite duration variance (§6).
- Fleet's history store and real p50. The planner was run with three calibration samples, not history.
- Executors with `executionOrder`, exclusive groups, shared state and per-device limits in a dynamic queue (§5).
- Device recovery after a kill. §2.4 shows the failure and that the device later worked. The exact recovery condition was not traced, and no device reset was tested.
- Two devices only. Scaling beyond two devices was not measured.
- Host contention: another agent's emulator (`emulator-5600` and a process named `fleet-fixtu…`) ran on the same host during the runs, and the 1-minute load average was 14 to 15. Spread is partly from that. The machine state was recorded per run, but only from the second rep onwards (`loadavg_1min_at_end` in `result.json`).
- Three repetitions per mode is the brief's minimum. It cannot separate `shard-split` from `static` (§3).

---

## 9. Product and adoption (checked 2026-11-09)

### 9.1 Friction against the planner's matrix output [I, not measured]

The planner writes a GitHub Actions matrix (`matrix-2.json` is `{"include":[{"shard":0},{"shard":1}]}`). That fits GitHub Actions directly. GitLab and Jenkins need the same list rendered into their own job definitions. A coordinate mode adds requirements on top of that. Every job must be able to reach the server with a token, and the workers must stay alive until the queue is empty, which fits a matrix job only if the matrix is sized in advance. In a dynamic queue, a job that starts late has no work. On all three providers the job count is fixed when the pipeline starts, so a dynamic queue does not change how many jobs run. It only changes which flows each job runs. The friction is mainly the new server dependency and the token in every job, not the matrix itself.

### 9.2 Marathon Cloud, current docs [V, fetch; checked against the raw repo]

- **Maestro support.** "Marathon Cloud supports Maestro for iOS and Android." and "sequential flow logic is not supported, as it conflicts with Marathon's design to complete all tests in under 15 minutes." Each Maestro flow is a separate test (raw `docs/cloud/intro/maestro.md` at `34e5d3d0e6`, the same HEAD research 11 read; re-read 2026-11-09, unchanged). [https://github.com/MarathonLabs/marathon/blob/develop/docs/cloud/intro/maestro.md]
- **Scheduling.** The Cloud page does not describe sharding or a scheduling mechanism. The 15-minute target and the "40 to 80 seconds, and no more than 3 minutes" per-flow guidance are its only timing statements. [same URL]
- **Pricing.** Live page: Android "$2.00 per hour per virtual device", iOS "$3.00 per hour per virtual device", "second-level precision", "50 free device hours on signup", "Last updated: Nov 7, 2026". [https://docs.marathonlabs.io/cloud/account/pricing/, fetched 2026-11-09]. The raw file at `34e5d3d0e6` (`docs/cloud/misc/pricing.md`) still quotes "$2 per hour for each virtual device" only, with no iOS price. The raw file is older. Both are consistent with research 11's figures; the live page is newer. [https://github.com/MarathonLabs/marathon/blob/34e5d3d0e6/docs/cloud/misc/pricing.md]
- **Retries.** Reactive: "When a test fails, it is scheduled again on a different device". Preventive: the scheduler "may run it more than once from the start". Uncompleted: a test with no result, such as one from a device crash or batch timeout, is re-run under its own quota. "A test that passes on any attempt is reported as **flaky**" and "Flaky tests count as passing for the run's exit code." Quotas are set by the platform from suite history and can be overridden. [https://docs.marathonlabs.io/cloud/configuration/retries/, fetched 2026-11-09]

Marathon Cloud's answer to I3 is "flaky, but passing". Fleet's I3 is stricter: a retried-then-passed flow may not be plain passed. Both can coexist, but Fleet cannot copy Marathon's exit-code rule.

### 9.3 Maestro Cloud, dynamic scheduling and retries [V, fetch]

- **Retries.** "Smart retries": Maestro Cloud "automatically retry[s] tests that we consider might be a flaky test", when the previous run of the same flow (by `name`, or filename) succeeded and the run used the same device model. Infrastructure failures are retried "on a different machine". The maximum number of retries and whether a flow that passes on retry is labelled flaky are not on the page. [https://docs.maestro.dev/maestro-cloud/advanced-features/automatic-retries, fetched 2026-11-09]
- **Dynamic scheduling and sharding.** Not described on the pages read. The Maestro run page (`docs.maestro.dev/cloud/run-maestro-tests-in-the-cloud`) has no sharding or work-queue section. A search excerpt from `cloud.mobile.dev/reference/execution-order` says flows run in parallel by default, and that listing a flow in `config.yaml` runs it in sequence; that excerpt is third-party and not verified against the page [V3]. Maestro Cloud does not document a dynamic queue on the pages read.
- **Pricing.** Not checked. It is not in the brief.

### 9.4 What this means for Fleet [I]

Two commercial tools do retries on their own devices. Neither documents a dynamic queue over a company's own runners, which is still the gap research 11 described. Maestro Cloud's retry rule, rerun on a different device on infrastructure failure, is the closest thing to Fleet's I2 and I3 questions. It does not settle them.

---

## 10. Sources

- Raw repo, Marathon `34e5d3d0e6`: `docs/cloud/intro/maestro.md`, `docs/cloud/misc/pricing.md`, read via the GitHub API on 2026-11-09.
- Marathon Cloud docs, fetched 2026-11-09: https://docs.marathonlabs.io/cloud/account/pricing/ and https://docs.marathonlabs.io/cloud/configuration/retries/
- Maestro Cloud docs, fetched 2026-11-09: https://docs.maestro.dev/maestro-cloud/advanced-features/automatic-retries and https://docs.maestro.dev/cloud/run-maestro-tests-in-the-cloud (no retry or scheduling detail on that page).
- Search excerpt, 2026-11-09 (V3, not verified): `https://cloud.mobile.dev/reference/execution-order`.
- Measurements: [`13-assets/`](13-assets/README.md), with `bench.py` and `results/*/result.json`.
- Fixtures: `fixtures/maestro/2.12.1/suite/stdout.bin`, `shard-split-2/stdout.bin` (research 08).
- Code: `crates/fleet-core/src/claims.rs`, `crates/fleet-core/src/plan.rs`, `crates/fleet-planner/src/main.rs`, `crates/fleet-store/src/ingest.rs`.
- Decisions: [ADR-0002](../docs/decisions/0002-agent-supervisor-observer.md), [ADR-0013](../docs/decisions/0013-fleet-planner.md).
