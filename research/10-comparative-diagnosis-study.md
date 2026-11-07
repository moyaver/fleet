# 10: Comparative diagnosis study

**Research date:** 2026-11-07 · **Issue:** [#80](https://github.com/moyaver/fleet/issues/80) · **Decides:** which M7+ diagnosis work is worth building, and which positioning claims are honest · **Builds on:** [08 Phase 0 results](08-phase0-results.md) (the Allure misclassification), [09 Competitive review](09-competitive-review-2026-11.md) item 11 · **Fixtures:** [`fixtures/maestro/2.12.1/`](../fixtures/maestro/2.12.1/) · **Fleet build:** `origin/main` at `97a956b`, `cargo build -p fleet-agent` (debug)

Basis labels used in every table: **M** = measured here by running the tool on the captured fixture output; **D** = documented (official docs or plugin source, cited), not run; **I** = inferred from the measured output.

## 1. Bottom line

1. **Allure 3.33.0 still shows Maestro failures as passes.** Re-tested on research 08's exact input (the `suite`, `shard-split-2` and `device-killed` JUnit files): 5 unique tests, 4 retries, 5 passed, the same result as research 08. On every failing or erroring flow in this set (5 of 5) it reports `passed`. The cause is in the reader: a text-only `<failure>` element is never read as a failure (section 7).
2. **CI-only reporting gets the outcome right in every scenario** (`[Passed]`/`[Failed]` lines and exit code; the cancelled run needs its exit code 130 or 143 decoded), but it has no evidence. It names the wrong cause for the app crash (`crash`), and it cannot show the screen behind any assertion failure.
3. **ReportPortal's JUnit import would also get every outcome right, but documented, not measured.** Its plugin reads `<failure>`/`<error>` as FAILED and logs the failure text. It drops the `device` attribute, has no attachment path for Maestro output, and imports nothing for an interrupted run (section 8).
4. **Fleet reaches the decisive evidence in 7 of 8 scenarios** (the next action is identifiable from its output), against 3 of 8 for CI-only, 2 of 8 for Allure and 3 of 8 for ReportPortal (documented). It matches Maestro's outcome in all 13 flows of the 10 captured runs it was given. It is worse than the others on the app crash: it does not upload `crash-report.txt`, and it names the assertion as the cause (section 9).
5. **Fleet's losses are real.** It does not label device loss as infrastructure in its inspect output, it does not identify the crash, it does not upload logcat, and the crash screenshot is missing from the fixture so what it shows is not measured. Its setup cost (an agent wrapping Maestro, plus a server) is not measured and is higher than CI-only, which needs nothing.

## 2. Scenarios and ground truth

Eight scenarios from the captured 2.12.1 fixtures, plus a control. Ground truth is what Maestro's own files show. The correct next action is what a developer should do first.

| ID | Fixture (under `fixtures/maestro/2.12.1/`) | What happened (source) | Correct next action |
|---|---|---|---|
| S0 | `pass` | Passed, exit 0 (`pass/exit.json`) | None |
| S1 | `fail` | Assertion `"Fleet never shows this" is visible` failed at step 5, exit 1 (`fail/exit.json`, `fail/run/report.xml:4-5`) | Look at the failing screen, then decide: fix the flow or the app |
| S2 | `device-killed` | `slow` failed because the device server died on `emulator-5602` (`DeviceServerDiedException`, `device-killed/run/report.xml:5`), exit 1 | Treat as infrastructure: check the device, rerun. Do not edit the flow |
| S3 | `sigint` | Run cancelled by SIGINT after 30 s, exit 130, no `report.xml` (`sigint/exit.json`, `sigint/run/`) | Rerun. No test result exists |
| S4 | `shard-split-2` | Two shards on two emulators. `delta-fails` failed on shard 1 (`emulator-5600`); `bravo`, `charlie`, `alpha` passed (`shard-split-2/stdout.bin`, `shard-split-2/run/report.xml`) | Look at `delta-fails`' screen (same as S1) |
| S5 | `nested` | Nested `runFlow`, plus a `retry` whose first attempt failed (`Attempt 1 failed for retry command`, `nested/run/debug/maestro.log`). Flow passed, exit 0 | None (optionally note the flake) |
| S6 | `retry-fail` | Retry exhausted, assertion failed twice, exit 1 (`retry-fail/run/debug/maestro.log`, `retry-fail/run/report.xml`) | As S1 |
| S7 | `crash` | Settings app crashed (`CrashedByAdbException: shell-induced crash`, `crash/run/debug/crash/logs/crash-report.txt:2`). The flow then failed a 20-second wait for text that never appears (`crash/stdout.bin`, exit 1) | Read the crash report. This is an app crash, not an assertion problem |

Not used, with reason: `sigterm` and `sigkill` (same cancellation class as S3; measured in Fleet only, section 9); `shard-all-2` (used only to confirm the Allure duplicate folding, section 7); `features`, `verbose`, `recording` and others (not needed for the six classes the issue lists). A nested run that **fails** has no fixture; the nested case here is a pass with an internal retry failure, so no claim is made about failing nested flows.

## 3. Method, counting rules and rubric

**Inputs.** Each tool gets the same captured output that a CI job would produce: CI-only gets the console (`stdout.bin`, `stderr.bin`) and exit status (`exit.json`); Allure and ReportPortal get the JUnit file (`run/report.xml`) because that is their native import format and they cannot read Maestro's debug folder; Fleet gets the capture directory (`fleet-agent inspect`).

**Allure 3.33.0.** `npm install allure@3.33.0` (npm `latest` at the time of the run, `dist-tags`: `latest: 3.33.0`), then `allure generate in --output out` on a copy of each report. Run from a scratch directory; output was read from `out/data/test-results/*.json` and `out/widgets/statistic.json`. The Allure HTML was **not** opened in a browser, so what the page renders is inferred from those data files, which the page reads.

**ReportPortal.** Not deployed. `docker system df` showed about 4.8 GB of images, 33 GB of volumes (81% reclaimable) and 11 GB of build cache, and the Docker VM already runs other agents' Postgres containers. Its full stack is about ten containers, so it was not run. Every ReportPortal cell is **D**, from the official JUnit plugin page and the plugin source (section 8).

**Fleet.** `target/debug/fleet-agent inspect --json --events <fixture dir>` for each capture. Outputs were compared mechanically with the snapshots in `crates/fleet-agent/tests/it/snapshots/it__inspect__<scenario>.snap`; flows (shard, name, outcome, finality, source, steps, failure) and event counts match for all ten. Dashboard, server and history were not run, so Fleet numbers are about what the agent's own outcome and artifacts contain.

**Result correctness rubric.** *Outcome correct*: the tool's result for the flow matches Maestro's (`passed`, `failed`, or an interrupted run). *Cause correct*: the failure text or status names the real cause from the ground truth in section 2 (assertion, device loss, crash, or cancellation). Each is correct, partial (right in part, e.g. says `failed` but names no cause), wrong, or none (nothing reported).

**Evidence available.** What a developer can open from the tool's own output for the failing step: failure text, screenshot, UI hierarchy, step list, log lines, logcat, crash report, device or shard identity.

**Steps to the correct next action (counting rule, fixed before measuring).** Start at the CI job page, which is already open. Each file opened, page or panel navigated, or search run counts as 1. Count opens until the tool's own output holds the evidence that decides the next action in section 2. If the tool never holds it, the row says **not reachable**, and the count is the opens up to the last view before the developer has to leave the tool. Wall-clock time is not measured: no UI session was run, so time claims are not made.

**Honest scope.** One fixture per scenario, one run each, no repetition, no statistics. Fleet's flake exclusion is code-read and unit-tested, not run end to end on these fixtures.

## 4. Result correctness

| ID | Truth | CI-only (M) | Allure 3.33.0 (M) | ReportPortal (D) | Fleet (M, `inspect`) |
|---|---|---|---|---|---|
| S0 pass | passed | correct: `[Passed] pass`, exit 0 | correct: `passed` | correct: PASSED | correct: passed |
| S1 fail | failed, assertion | outcome correct; cause partial (assertion text, no screen) | **wrong**: `passed` (statistic `{total:1, passed:1}`) | outcome correct; assertion text logged at ERROR | outcome correct; assertion message |
| S2 device-killed | failed, device lost | outcome correct; cause none (`[Failed] slow (32s)` has no reason) | **wrong**: `passed`, labelled `slow` | outcome correct; `DeviceServerDiedException` stack in the log; no infrastructure label | outcome correct; message names `DeviceServerDiedException`; **not labelled infrastructure** in inspect output (partial) |
| S3 sigint | cancelled | outcome partial: no per-flow line, only exit 130 (reader must map the code) | **wrong**: silent, `{total:0}`, `allure generate` exit 0 | no import without an XML (no result) | correct: `interrupted`, finality `inferred`, from `process_exit` |
| S4 shard-split-2 | 3 pass, `delta-fails` fails on shard 1 | correct, with `[shard 1]` prefix; device serial not printed | **wrong** for `delta-fails` (`passed`); `{total:4, passed:4}` | correct FAILED for `delta-fails`; `[shard 1]` kept in the text; device attribute not read | correct for all 4; shard-to-device from the log (`emulator-5600`, `emulator-5602`) |
| S5 nested | passed (one retry attempt failed) | correct pass; retry invisible | correct pass; retry invisible | correct pass; retry invisible (not in JUnit) | correct pass; **the failed attempt is visible** (step 14 failed, with screenshot and hierarchy) but not labelled as a retry (partial on labelling) |
| S6 retry-fail | failed after retry | correct outcome; retry exhaustion not stated | **wrong**: `passed` | correct FAILED | correct: failed, two failed assertions, three screenshots |
| S7 crash | app crash (assertion is a symptom) | outcome correct; **cause wrong** (points at the assertion) | **wrong**: `passed` | outcome correct; **cause wrong** (JUnit has only the assertion) | outcome correct; **cause wrong** (assertion message); crash report not uploaded |

Totals on the outcome column: CI-only 8 of 8 (S3 needs the reader to map exit 130); Allure 2 of 8; ReportPortal (D) 7 of 8 (no result for S3); Fleet 8 of 8.

## 5. Evidence available

| ID | CI-only (M) | Allure 3.33.0 (M) | ReportPortal (D) | Fleet (M) |
|---|---|---|---|---|
| S0 | exit code, pass line | test name, duration; no steps | test name, status | 7 steps, no artifacts |
| S1 | assertion text | none: `error: {}`, `steps: []`, `attachments: []` | assertion text (ERROR log); no screenshot (no attachment element in the XML tag list) | assertion text, **failure screenshot and hierarchy** (`artifact.registered`, step 5) |
| S2 | none; no reason printed | none; device attribute dropped | full stack in the log; no device | full stack; `commands_not_final` warning; device `emulator-5602` (`shard.assigned`); no screenshot (none written for the lost device) |
| S3 | exit 130 | nothing | nothing | `run.signal`, `process.exited` 130, 13 steps, `commands_not_final` warning |
| S4 | `[shard 1]` prefix on the result | 4 test entries, no shard | `[shard 1]` in failure text; no device | per-shard device serials; screenshot and hierarchy for `delta-fails` |
| S5 | none on the retry | none | none | failed step 14 with screenshot and hierarchy (raw name `Assert that "${output.target}"`, not expanded) |
| S6 | none beyond message | none | assertion text | three screenshots (incl. `step-004-retry.png`), two hierarchies, step list |
| S7 | assertion text only (misleading) | none | assertion text only | assertion screenshot and hierarchy registered; `crash-report.txt` **not uploaded**; the 1.25 MB failure screenshot is listed in `crash/SKIPPED.txt`, so what it shows is **not measured** |

## 6. Steps to the correct next action

Counted with the rule in section 3. "Not reachable" means the decisive evidence is not in the tool's output.

| ID | CI-only (M) | Allure 3.33.0 (M) | ReportPortal (D) | Fleet (M) |
|---|---|---|---|---|
| S0 pass | 1 (console) | 2 (report index, test page) | 2 (launch, item) | 1 (inspect) |
| S1 fail | 1; screen **not reachable** | 3 (index, test, console); says passed, so the developer is misled | 3 (launch, item, log); screen **not reachable** | 2 (inspect, screenshot) |
| S2 device-killed | 1; cause **not reachable** | 3; wrong | 3 (log stack shows device death) | 1 (inspect message names device death) |
| S3 sigint | 2 (console, exit code) | 2 (index shows 0 tests, console) | 2 (launch list empty, console) | 1 (inspect) |
| S4 shard-split-2 | 1; screen **not reachable**; device not reachable | 3; wrong | 3; screen and device **not reachable** | 2 (inspect, `delta-fails` screenshot) |
| S5 nested | 1 | 2 | 2 | 1 (step list shows the failed attempt; flow passed) |
| S6 retry-fail | 1; screen **not reachable** | 3; wrong | 3; screen **not reachable** | 2 (inspect, screenshot) |
| S7 crash | 1; **wrong** next action (fix the assertion) | 3; wrong | 3; wrong | 2 (inspect, screenshot); cause **not reachable** |

Decisive evidence reached (the next action can be chosen from the tool's output): CI-only 3 of 8 (S0, S3, S5), Allure 2 of 8 (S0, S5, and only because they are passes), ReportPortal (D) 3 of 8 (S0, S2, S5), Fleet 7 of 8 (all except S7).

Fleet is **not** fewer clicks for the simple failures: S1 costs 2 opens against 1 for CI-only. The gain is that its second open holds the evidence the first two tools cannot give.

## 7. Allure 3.33.0: re-test of research 08's misclassification

**Re-test on the research 08 input (M).** `allure generate` over `suite/run/report.xml`, `shard-split-2/run/report.xml` and `device-killed/run/report.xml` together:

- 9 test entries; 5 unique tests (`bravo`, `charlie`, `delta-fails`, `alpha`, `slow`); 4 entries marked `isRetry: true`; `statistic.json` `{ "total": 5, "retries": 4, "passed": 5 }`.
- `delta-fails` and `slow` have `status: "passed"` and `error: {}`.
- Research 08 recorded the same result on 3.22.1: 5 of 5 passed, 4 retries. **The misclassification still happens on 3.33.0.**

**Other scenarios (M).** Allure reports `passed` for `fail`, `crash`, `retry-fail`, `device-killed` and the shard `delta-fails`, which are the five failing or erroring flows in this set. `pass` and `nested` are correctly `passed`. For `shard-all-2` it folds the eight entries into four unique tests flagged as retries, which is the same folding research 08 saw. For `sigint`, which has no `report.xml`, the report is empty (`total: 0`) and the generator exits 0 with no message.

**Root cause (read in the 3.33.0 package, `@allurereport/reader` `dist/junitxml/index.js`).** Allure's JUnit reader decides status from child elements. `getStatus` (line 171) calls `maybeParseStatus("failed", failure)` (line 174), which only returns a status when the element is the empty string or a record with attributes (`isEmptyElement` / `isStringAnyRecord`, `dist/xml-utils.js` lines 1-8). Maestro writes `<failure>Assertion is false: ...</failure>`: text only, no attributes. The parser yields a plain string, neither predicate holds, `maybeParseStatus` returns `undefined`, and `getStatus` falls through to `{ status: "passed" }`. The `message` and `trace` are dropped with it.

**Diagnostic (synthetic, UNVERIFIED as a fixture; not committed).** A copy of the `fail` JUnit with `<failure message="Assertion is false">` added (the same text otherwise) gives `status: "failed"` and `error: {message, trace}`. So the misclassification is caused by the text-only `<failure>` form, which Maestro 2.12.1 always writes.

**Upstream state (D).** The status logic is in Allure's own reader, so it is a bug in the Allure project, not in Maestro's output. No upstream issue was searched for in this study.

**What Allure can still show.** The test's name, duration and status, the suite label, and the retries. No failure text, no steps, no attachments (all zero in the generated JSON for every failing scenario). A Maestro screenshot could be attached by hand through JUnit `system-out` tricks, but that is glue this study did not build and did not measure.

## 8. ReportPortal JUnit import: documented, not measured

Every cell in this section is **D**. Sources were read via the official docs and the plugin source; the source was read through a fetch tool that summarises pages, so the exact quotes below should be re-checked before they are used in public.

- **Endpoints and input.** Creating a launch: `POST /v1/plugin/{projectName}/junit/import` with an XML or ZIP file. Appending to an existing launch uses a JSON `launchImportRq`. The UI accepts `.xml` or `.zip` under 32 MB, and a non-JUnit file is rejected in red. Source: [reportportal.io JUnit plugin docs](https://reportportal.io/docs/plugins/import/JUnit/).
- **Status rule.** In `XunitImportHandler.java` (plugin-import-junit tag `1.0.0`, Apache-2.0), a `<failure>` or `<error>` child sets FAILED, `<skipped>` sets SKIPPED, and with no child the item is PASSED. The testcase `status="ERROR"` attribute that Maestro writes is never read. Source: [XunitImportHandler.java](https://raw.githubusercontent.com/reportportal/plugin-import-junit/1.0.0/plugin-junit/src/main/java/com/epam/reportportal/extension/importing/service/XunitImportHandler.java).
- **Failure text.** The text of `<failure>` (not its `message` attribute) is collected and logged at ERROR. This is the Maestro assertion text, or the `DeviceServerDiedException` stack, in the item's log. Same source.
- **Attributes read.** `XunitReportTag.java` lists the tags (`testsuites`, `testsuite`, `testcase`, `failure`, `error`, `skipped`, `system-out`, `system-err`, `properties`, `property`) and attributes (`name`, `time`, `type`, `message`, `value`). Maestro's `device` attribute on `<testsuite>` is not among them, so the device name is not kept. Source: [XunitReportTag.java](https://raw.githubusercontent.com/reportportal/plugin-import-junit/1.0.0/plugin-junit/src/main/java/com/epam/reportportal/extension/importing/service/XunitReportTag.java).
- **Known gap.** The `message` attribute on `failure` is not honoured. [Issue #2377](https://github.com/reportportal/reportportal/issues/2377) (opened 2024-10-14) reports it, and the issue was closed as completed on 2024-11-11 with a workaround (customise the plugin) and no fix. Not relevant to Maestro's text-only `<failure>`, but it confirms the importer's limits.
- **Interrupted runs.** No XML means no import. The import is end-of-run only, so a cancelled run leaves nothing. Source: the plugin page and the endpoint definition above (I).
- **Skipped items.** A third-party PR reports suite items without a status can end FAILED if a child is not PASSED/INFO/WARN ([7K-Inari PR #147](https://github.com/7K-Inari/inari-release-bundle/pull/147), a secondary account, not verified against the plugin). Not relevant to these fixtures, which have no skipped items, but it is a risk for Maestro `WARNED`/`SKIPPED` steps.

Cells in sections 4 to 6 marked ReportPortal (D) follow from these rules and the JUnit files. They are not measured.

## 9. Fleet: measured, with its losses

**Wins measured (M).**

- **Outcomes match Maestro in all 13 flows** across the 10 captured runs (`crates/fleet-agent/tests/it/snapshots/it__inspect__*.snap`, re-run here).
- **Cancellation is named.** SIGINT, SIGTERM and SIGKILL runs are `interrupted` with `finality: inferred` and source `process_exit` (`it__inspect__sigint.snap`, `sigterm.snap`, `sigkill.snap`). A `run.signal` event records the signal.
- **Failing steps carry evidence.** `fail`: a failure screenshot and a hierarchy for step 5 (`artifact.registered`). `retry-fail`: three screenshots and two hierarchies, including the retry step's screenshot (`step-004-retry.png`).
- **Shard-to-device is right.** `shard-split-2`: shard 1 is `emulator-5600`, shard 2 is `emulator-5602` (`shard.assigned`), taken from Maestro's `Selected device` log line. JUnit gives the same AVD name to both.
- **A failed attempt in a passing flow is visible** (`nested`, step 14). Not labelled as a retry (research 09's "retry semantics not distinct", tracked in #38).
- **Device loss names its cause** (`device-killed`, `agent.warning commands_not_final`). The flake exclusion for device loss is in `crates/fleet-core/src/flaky.rs:42-47`, with a unit test at `flaky.rs:420`. It is code-read and tested, not run on these fixtures.

**Losses measured (M).**

- **The crash is not identified.** `crash` is `failed` with the assertion message (`it__inspect__crash.snap`). The crash report (`logs/crash-report.txt`, `CrashedByAdbException`) is not uploaded: the artifact allow-list has no class for it (`crates/fleet-protocol/src/artifact.rs`, `ArtifactClass`), and no code path in `crates/fleet-agent/src/session.rs` registers it. Logcat (`logs/device-logcat.txt`, 1.6 MB per the manifest) is not uploaded either.
- **The crash screenshot is not in the fixture.** `crash/SKIPPED.txt` lists the 1.25 MB screenshot as over the import limit. The screenshot Fleet registers in that replay is the event, not a file, so what it shows is not measured.
- **Replay limitation.** In the `crash` replay the JUnit file was not read (`report.parsed` `status: absent`), because the capture's argv is `sh -c …` and `--output` cannot be found (`inspect.rs` `report_path`). The outcome came from the bundle manifest (`source: artifact_file`). A live `fleet-agent run` would not use that wrapper, but this was not measured live. This is an open item for the owner, not a verdict on the live path.
- **Device loss is not labelled infrastructure in the inspect output.** The outcome is `failed` with source `junit_final`. The infrastructure exclusion applies to flake history, not to the displayed outcome, so a developer sees `failed`.
- **Setup cost is not measured.** Fleet needs the agent to wrap Maestro and a server to see the dashboard. CI-only needs nothing. This study measures what each tool shows, not what it costs to run.

## 10. Wins and losses, by scenario

| ID | Fleet vs CI-only | Fleet vs Allure 3.33.0 | Fleet vs ReportPortal (D) |
|---|---|---|---|
| S0 | equal | equal | equal |
| S1 | **win**: screen evidence reachable | **win**: Allure says passed | **win**: screen evidence (RP has none via import) |
| S2 | **win** on cause (message); **loss** on labelling (not `infrastructure`) | **win** | **equal** (both name the death; neither labels infra) |
| S3 | **win** on classification (`interrupted` vs exit code to decode); equal on cause | **win** (Allure is silent) | **win** (RP has no result) |
| S4 | **win** on device per shard; equal on shard name | **win** | **win** on device; equal on outcome |
| S5 | **win**: failed attempt visible; **loss**: not labelled as retry | **win** | **win** |
| S6 | **win** on evidence (three screenshots); equal on outcome | **win** | **win** on evidence |
| S7 | **loss**: equal on cause (both wrong), **and** Fleet does not upload the crash report | **win** on outcome | **equal** on cause (both wrong); Fleet has a screenshot, RP has none |

Summary: Fleet wins on evidence and on classification in S1, S3, S4, S5 and S6. It is equal to CI-only and ReportPortal on the outcome everywhere, and it is equal or worse than CI-only and ReportPortal on the cause in the app crash (S7) and the infrastructure label (S2).

## 11. Claims the README may make

Each claim is tied to a measured row.

1. "Fleet reports the same outcome as Maestro's own JUnit for every captured scenario in the fixture set (13 flows, 10 runs)." (Section 9, outcome match; snapshots.)
2. "Allure 3.33.0 shows a failing Maestro JUnit testcase as passed." Limit it to the JUnit file: Allure 3.33.0 showed 5 of 5 failing flows as passed (section 4, S1, S2, S4, S6, S7; section 7). Research 08's input reproduces exactly.
3. "Allure 3.33.0 generates an empty, exit-0 report for a run that was cancelled before its JUnit was written." (Section 4, S3.)
4. "Fleet keeps the failure screenshot and UI hierarchy for a failing Maestro step." (Section 5, S1, S4, S6; `artifact.registered`.)
5. "Fleet shows which emulator ran which shard, from Maestro's log; the JUnit file gives both shards the same AVD name." (Section 9, `shard-split-2`.)
6. "Fleet records a SIGINT, SIGTERM or SIGKILL run as interrupted, not failed." (Section 9, `sigint`, `sigterm`, `sigkill` snapshots.)
7. "Fleet shows a failed retry attempt inside a flow that passed." (Section 4, S5, step 14. Do not say "labels" it.)
8. "In the captured fixtures, CI-only reporting and JUnit-based imports cannot show the screen at a failing step." (Section 5, CI-only and Allure rows; ReportPortal row is documented, so say so.)

## 12. Claims the README may not make

1. "Fleet diagnoses crashes." It names the assertion for `crash` (section 4, S7), and it does not upload the crash report (section 9).
2. "Fleet classifies device loss as infrastructure." Its inspect output says `failed` (section 9). The exclusion exists in flake history only, and was not run end to end here.
3. "Fleet is faster to the fix" or "fewer clicks than CI-only." Section 6 shows Fleet costs 2 opens against 1 for a simple failure, and no time was measured.
4. "Fleet is better than ReportPortal" or "ReportPortal misses X." ReportPortal was not run. Its cells are documented, and the only firm statements are the ones in section 8, which describe what its import reads, not what it shows in its UI.
5. "Allure is broken" in general, or "Allure can't show screenshots." The defect is in the JUnit reader for text-only `<failure>` elements. Allure's other inputs were not tested, and attachments were not tried.
6. "Maestro's output is wrong." Maestro writes a standard JUnit `<failure>`. The misreading is in the tools that parse it.
7. Any Fleet UI, dashboard, history or flake-rate statement. None of them was run for this study.
8. "Fleet is the only tool that…" unless it is limited to the tools and scenarios in sections 4-6.
9. Any claim about the crash screenshot's content (`crash/SKIPPED.txt`).

## 13. Reproduce, and what is not done

**Commands.**

```bash
# Allure 3.33.0 (scratch dir, not in the repo)
npm install allure@3.33.0
allure generate in --output out            # in/ holds one report.xml per scenario
# read out/data/test-results/*.json and out/widgets/statistic.json

# Fleet
cargo build -p fleet-agent --bin fleet-agent
target/debug/fleet-agent inspect --json --events fixtures/maestro/2.12.1/<scenario>
```

**Not done.** ReportPortal was not deployed or run (section 3). The Allure HTML was not opened in a browser (section 3). No wall-clock timing. No Fleet dashboard or server run. No repetition of any run. The `crash` screenshot was not captured (`crash/SKIPPED.txt`). `sigterm` and `sigkill` were not run in Allure. No `ANR` fixture exists (`fixtures/maestro/README.md`, `anr` not captured). Failing nested flows have no fixture.

**Open items for the owner.**

- Fleet: `inspect` loses the JUnit cross-check for `crash`, because the capture's argv is `sh -c` (section 9). Decide whether to make `report_path` work through `sh -c`, or to re-capture the crash without the wrapper.
- Fleet: `crash-report.txt` and logcat are not uploaded (section 9). Adding a crash class needs a redaction rule (invariant I6, research 09 section 4.1).
- Fleet: device loss needs a visible infrastructure label in the outcome, not only in flake history.
- Upstream: a short Allure issue quoting `dist/junitxml/index.js` `maybeParseStatus` would be a useful, bounded report. Not filed (an owner decision).
