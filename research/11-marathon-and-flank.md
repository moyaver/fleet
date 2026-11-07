# 11: Marathon and Flank as prior art

**Research date:** 2026-11-07
**Scope:** Marathon (open-source runner `MarathonLabs/marathon` and hosted Marathon Cloud) and Flank (`Flank/flank`, a Firebase Test Lab wrapper), as prior art for Fleet Planner's duration-aware sharding, Fleet's history and flakiness features, and Maestro support. Primary sources only: repository source and docs at the HEADs below, GitHub API metadata, vendor docs, and the Firebase Test Lab pricing page.
**Builds on:** [07 Prior art and build-vs-buy](07-prior-art-and-build-vs-buy.md), [09 Competitive review](09-competitive-review-2026-11.md), [ADR-0013](../docs/decisions/0013-fleet-planner.md)

## How to read this report

Markers follow research 09:

- **[V]**: verified today against a primary source. The URL or repository path (with line where useful) is given.
- **[V3]**: third-party or search-excerpt only. Not used here.
- **[I]**: inferred, or not verifiable today. Treat as a hypothesis.

Two source types need a caveat. **Repo HEADs read:** Marathon `34e5d3d0e6` (develop, 2026-09-21); Flank `38d9c86b9f` (master, 2026-02-25). **Fetch summary:** vendor pages that were not in the repo (Marathon Cloud pricing, retries and setup; Firebase pricing) were read through a page-summarising fetch. Their numbers are marked **[V, fetch]** and should be checked by hand before any budget decision. `marathonlabs.github.io` and `flank.github.io` could not be fetched; the same content was read from each repo's `docs/` source instead.

---

## 1. Bottom line

1. **Neither open-source runner supports Maestro.** Marathon mentions Maestro only in its Cloud docs; the open-source runner has no Maestro code or docs **[V]** (`docs/cloud/intro/maestro.md` and `docs/cloud/sidebars.js` are the only hits in the full tree). Flank has no Maestro reference anywhere in its tree **[V]**. The self-hosted Maestro niche is still unclaimed by both **[I]**.
2. **Marathon Cloud runs Maestro, on its own devices.** It supports Maestro for Android and iOS **[V]** (`docs/cloud/intro/maestro.md:7`). Each flow is one test, and **sequential flow logic is not supported** **[V]** (`docs/cloud/intro/maestro.md:11-12`). Fleet Planner keeps `executionOrder` sequences together **[V]** (`crates/fleet-planner/src/main.rs:5`).
3. **The cross-job splits differ: Marathon's is count-only, Flank's is history-based.** Marathon's cross-job split (`FragmentationFilter`) is an md5 hash modulo a count, with no timing data **[V]** (`core/src/main/kotlin/com/malinskiy/marathon/execution/filter/FragmentationFilter.kt`). Marathon's in-run ordering uses an analytics backend's execution-time percentile **[V]**. Flank's Smart Flank uses JUnit timings from the previous run and estimates 10 s for unknown tests **[V]** (`docs/smart_flank.md:7`). Fleet Planner uses per-flow p50 with p90 as tie-break, and LPT across CI jobs **[V]** (`crates/fleet-planner/src/main.rs:135-136, 194`).
4. **Marathon has the most developed retry and flakiness model of the three.** Preventive retries come from a success-rate probability model; post-factum retries use quotas; a test that fails then passes is reported as flaky in Cloud **[V]**. Flank exposes Firebase's re-attempt count (max 10) and nothing else **[V]** (`docs/index.md:115-118`). Neither quarantines tests **[V]** on the pages read.
5. **Reporting is batch, not live.** Marathon writes JUnit, HTML, timeline and Allure reports, and can record video or screenshots per test **[V]**. Flank writes HTML and JUnit reports and an optional video **[V]** (`docs/index.md:28-29, 79-81`). Neither has a web dashboard or analytics server of its own; Marathon can push metrics to InfluxDB or Graphite **[V]**.
6. **Maintenance and cost differ.** Marathon: latest release 0.10.5 (2026-07-21), GPL-2.0, Cloud at $2 per Android and $3 per iOS virtual device-hour **[V, fetch]**. Flank: latest release v23.10.1 (2023-10-26), Apache-2.0, no own cloud; it bills through Firebase Test Lab at $1 per virtual and $5 per physical device-hour **[V, fetch]**. Flank's release gap is the maintenance signal **[I]**.

---

## 2. Marathon

### 2.1 What it is and how it runs

- **Open-source runner:** a Kotlin Android and iOS test runner, described on GitHub as a "Cross-platform test runner" **[V]** (GitHub API). It runs from a CLI or a Gradle plugin; the plugin is "a thin wrapper around CLI" **[V]** (`docs/runner/android/install.md`, caution block).
- **Devices:** local devices and emulators addressed by adb serial, as `adb devices` lists them **[V]** (`docs/runner/android/configure.md:263`). The overview also mentions "an API for use with custom hardware farms" **[V]** (`docs/runner/intro/overview.md:6`); no farm integration was found beyond that sentence **[I]**.
- **Marathon Cloud:** a hosted service with its own `marathon-cloud` CLI. It "provisions devices, distributes tests, and delivers results", with a stated target of 15 minutes per run **[V, fetch]** (`docs.marathonlabs.io/cloud/getting-started/overview`). Android runs always use `google_apis` system images **[V, fetch]** (`docs.marathonlabs.io/cloud/maestro/setup`).
- **Separate product:** "Emu", an Android emulator and device companion, is named on the Marathon docs site **[V, fetch]**. Not researched further.

### 2.2 Runner support

| Runner | Marathon open-source | Marathon Cloud |
|---|---|---|
| Android instrumentation (Espresso) | Yes **[V]** (`docs/runner/android/`). "Espresso" is named in one file in docs and code, so Compose-specific handling is **not found** | Not checked |
| iOS XCUITest and XCTest | Yes, detected from the `.xctest` bundle **[V]** (`docs/runner/apple/configure/macos.md:26, 57`) | Simulator builds only **[V, fetch]** |
| Maestro | **Not present** **[V]** (grep of the full tree at HEAD) | **Yes**, Android and iOS **[V]** (`docs/cloud/intro/maestro.md:7`) |
| Appium | **Not found** (0 hits in `docs/runner` and `core/src/main`) **[V]** | Not found |

### 2.3 Scheduling

- **Device pools:** a default `omni` pool, plus pooling by model, OS version, ABI, manufacturer, or a combination **[V]** (`configuration/src/main/kotlin/com/malinskiy/marathon/config/strategy/PoolingStrategyConfiguration.kt:19`; `docs/runner/index.md:69`).
- **Sharding inside one run:** `count` and `parallel` only **[V]** (`configuration/.../strategy/ShardingStrategyConfiguration.kt:13-14`). `parallel` is the default **[V]** (`docs/runner/configuration/sharding.md`). The docs say sharding is not run-splitting; for splitting across CI jobs, see fragmentation below **[V]**.
- **Cross-job split:** `fragmentation` filter, with `index` and `count`. The test is assigned by md5 of its name modulo `count` **[V]** (`docs/runner/configuration/filtering.md:492-507`; `FragmentationFilter.kt`). The docs say results are "sub-optimal" compared with several devices in one run **[V]**. **No duration or history input** **[V]**.
- **Sorting:** execution-time (by percentile and time limit, longest first), success-rate, random, none **[V]** (`SortingStrategyConfiguration.kt:14-17`; `ExecutionTimeSortingStrategy.kt`).
- **History source:** the analytics backend, through `MetricsProvider.executionTime(test, percentile, limit)` and `successRate(...)` **[V]** (`core/src/main/kotlin/com/malinskiy/marathon/analytics/external/MetricsProvider.kt:7-8`). Backends: InfluxDB v1, InfluxDB v2, Graphite **[V]** (`configuration/.../AnalyticsConfiguration.kt:13-16`). Default is disabled, so every test is new **[V]** (`docs/runner/configuration/analytics.md`).
- **Batching:** isolate (default), fixed-size, class-name **[V]** (`BatchingStrategyConfiguration.kt:18`; `docs/runner/configuration/batching.md`).
- **Retries:** post-factum with quotas (`FixedQuotaRetryStrategy`) or none **[V]** (`core/.../execution/strategy/impl/retry/`). Preventive retries use `ProbabilityBasedFlakinessStrategy`, which schedules extra runs to reach a target pass probability **[V]** (`docs/runner/configuration/flakiness.md`; worked example 0.5 becomes 3 runs for 0.8).
- **Flaky classification (open-source):** not traced **[I]**. Cloud reports fail-then-pass as Flaky **[V, fetch]**.
- **Filtering:** include and exclude lists, Allure-based filtering, annotation mapping **[V]** (`docs/runner/configuration/filtering.md`).

### 2.4 Output

- **Reports:** JUnit XML at `outputDir/tests/<pool>/marathon_junit_report.xml` **[V]** (`docs/runner/index.md:98`). HTML report, with `index.html`, pool pages and test pages **[V]** (`core/.../report/html/HtmlSummaryReporter.kt:65-68`). Timeline report **[V]** (`core/.../report/timeline/TimelineReporter.kt`). Allure report **[V]** (`core/.../report/allure/AllureReporter.kt`).
- **Video and screenshots:** Android screen-recording config chooses `VIDEO` or `SCREENSHOT` **[V]** (`configuration/.../vendor/android/ScreenRecordConfiguration.kt`). Attachment types include `video/mp4` and screenshot formats **[V]** (`core/.../execution/Attachment.kt:19-23`, `io/FileType.kt:10-15`).
- **Logcat:** a `LOGCAT` attachment constant exists **[V]** (`Attachment.kt:12`), and the HTML report renders the failing test's stack trace as log lines **[V]** (`HtmlSummaryReporter.kt:115, 138-148`). A logcat collector was **not traced** **[I]**.
- **Live view:** per-test progress lines in the terminal **[V]** (`docs/runner/index.md:69-70`). No web dashboard was found in the open-source runner **[I]** (not exhaustively searched).
- **Analytics server:** none of its own. Marathon writes to InfluxDB or Graphite **[V]** (`core/.../analytics/external/influx/InfluxDbTracker.kt`, `graphite/GraphiteTracker.kt`).

### 2.5 Maintenance

- **Latest release:** 0.10.5, published 2026-07-21, not a pre-release **[V]** (GitHub API). Previous releases: 0.10.4 (2025-04-18), 0.10.3 (2024-09-29). Cadence is about one release per year recently **[I]**.
- **Last commit:** 2026-09-21 on `develop` (`34e5d3d0e6`, "docs: rename back") **[V]** (GitHub API; clone HEAD). Repo `updated_at` 2026-10-30 **[V]**.
- **Archived:** no **[V]**. **License:** GPL-2.0 (GitHub SPDX `GPL-2.0`); the `LICENSE` file is GPL v2 text **[V]** (`LICENSE`). The file does not say "only" or "or later" **[I]**.
- **Stars:** 646. **Open issues:** 71 **[V]** (GitHub API, 2026-11-07).

### 2.6 Commercial offering

- **Marathon Cloud pricing:** pay-as-you-go by virtual device-hour. Android $2 per hour, iOS $3 per hour, billed to the second. Starter plan with 50 free device-hours on signup. Enterprise is custom **[V, fetch]** (`docs.marathonlabs.io/cloud/account/pricing`).
- **Maestro on Cloud:** yes **[V]** (`docs/cloud/intro/maestro.md`). Maestro flows use Linux-style paths, and `--maestro-env` passes variables **[V]**. `--filter-file` selects subsets by tag or path **[V, fetch]**.
- **Retries on Cloud:** three quota-based strategies: reactive (failed tests move to another device), preventive (unstable tests run more than once from the start), and uncompleted (re-run after device crashes or timeouts) **[V, fetch]** (`docs.marathonlabs.io/cloud/configuration/retries`). The flakiness history behind preventive retries is not shown in the console **[V, fetch]**.
- **Quarantine:** not found on the Cloud retries page **[V, fetch]**. Its guidance is to fix flakiness at the source.
- **Self-hosting:** not documented for Cloud **[I]**. Cloud devices are hosted by Marathon.

---

## 3. Flank

### 3.1 What it is and how it runs

- A Kotlin CLI, "massively parallel Android and iOS test runner for Firebase Test Lab" **[V]** (`README.md`). It is YAML-compatible with the `gcloud firebase test` commands and reimplements them **[V]** (`docs/flank_vision.md`).
- **Devices:** only Firebase Test Lab. Every run path in the docs is a Firebase Test Lab run **[V]** (`docs/index.md`). No local adb path was found **[V, absence]**.
- **Gradle:** Fladle, a third-party plugin, is referenced by the project **[V]** (`docs/flank_vision.md`).
- **Team status:** the vision page says the team is focused on bug fixes and stabilisation, undated **[V]** (`docs/flank_vision.md`).

### 3.2 Runner support

- **Android:** instrumentation (Espresso or UI Automator) and Robo and game-loop tests, as Firebase Test Lab lists them **[V, fetch]** (`firebase.google.com/docs/test-lab/overview`).
- **iOS:** XCTest through `.xctestrun`, and game-loop (`test-type` `xctest` or `game-loop`) **[V]** (`docs/index.md:133-142, 196`).
- **EarlGrey:** referenced in the Smart Flank design doc and samples **[V]** (`docs/smart_flank.md`).
- **Maestro, Appium, Flutter:** **not found** (0 hits in `docs/` and the full tree) **[V, absence]**. Firebase's overview page does not list Maestro, Appium or Flutter **[V, fetch]**.
- **Test enumeration:** Hypershard lists Android and iOS tests without a build **[V]** (`docs/hypershard_android.md`, `docs/hypershard_ios.md`).

### 3.3 Scheduling

- **Uniform sharding** (`num-uniform-shards`): Flank's own docs say it is "not recommended", because empty shards fail on Firebase **[V]** (`docs/test_sharding.md:35`).
- **Manual sharding:** `test-targets-for-shard` **[V]** (`docs/test_sharding.md`).
- **Smart Flank (history-based):** reads a JUnit XML with timings from the previous run. Unknown tests are estimated at 10 s. Tests go into equal-time buckets, and the bucket count is `maxTestShards` **[V]** (`docs/smart_flank.md:7`). The merged XML goes to `smartFlankGcsPath`, and without that path the feature is off **[V]** (`docs/smart_flank.md:11`). The design doc lists "merging XML files is complicated" as a known issue **[V]** (`docs/smart_flank.md`, Issues).
- **Retries:** `num-flaky-test-attempts`, default 0, maximum 10. Firebase may re-attempt on suspected infrastructure problems **[V]** (`docs/index.md:115-122`). A fail-fast option limits this **[V]**.
- **Flaky tests:** "full JUnit result" adds failure nodes to passed flaky tests **[V]** (`docs/index.md:311-312`). No flaky classification of its own was found **[V, absence]**.
- **Batching, quarantine, probabilistic retries:** not found **[V, absence]**.

### 3.4 Output

- **Reports:** HTML report and JUnit XML **[V]** (`docs/index.md:28-29`). A JUnit-to-HTML module is in the repo **[V]** (`junit_html_report/`).
- **Video:** `record-video`, off by default **[V]** (`docs/index.md:79-81`).
- **Screenshots and logcat:** not found in Flank's docs **[V, absence]**. Firebase may keep these as artifacts; not verified **[I]**.
- **Live view:** terminal output styles `verbose`, `multi`, `single` and `compact` **[V]** (`docs/index.md:306-310`). Results and history go to the Firebase console; the history list is time-ordered **[V]** (`docs/index.md:112`).
- **Analytics server:** none. Timing history is a JUnit file in a GCS bucket **[V]** (`docs/smart_flank.md`).

### 3.5 Maintenance

- **Latest release:** v23.10.1, 2023-10-26, not a pre-release **[V]** (GitHub API). Release notes end at the same version **[V]** (`release_notes.md`). Tags: 30 in total, newest v23.10.1 **[V]** (GitHub API).
- **Last commit:** 2026-02-25 on `master` (`38d9c86b9f`, "Refactor Github Action per b/485167538 (#2616)") **[V]** (GitHub API; clone HEAD). GitHub's `pushed_at` is 2026-06-20 **[V]**, which points to a non-default branch; not identified **[I]**.
- **Archived:** no. **License:** Apache-2.0 **[V]** (`LICENSE`; GitHub SPDX `Apache-2.0`).
- **Stars:** 701. **Open issues:** 74 **[V]** (GitHub API, 2026-11-07).

### 3.6 Commercial offering

- **No Flank-owned cloud** **[V, absence]**. Costs are Firebase Test Lab's.
- **Firebase Test Lab (Blaze plan):** $5 per physical device-hour, $1 per virtual device-hour, billed by test-run minute, rounded up. Free quotas: Spark plan 15 runs per day; Blaze 30 minutes per day physical and 60 minutes virtual **[V, fetch]** (`firebase.google.com/docs/test-lab/usage-quotas-pricing`). Per-minute rates and iOS rates were **not found** on the page read.
- **Maestro on Firebase Test Lab:** not found **[V, fetch]**.

---

## 4. Comparison

| | Marathon (open source) | Marathon Cloud | Flank | Fleet |
|---|---|---|---|---|
| Form | CLI and Gradle plugin | Hosted, `marathon-cloud` CLI | CLI wrapping Firebase Test Lab | Rust agent, server, dashboard, `fleet-planner` CLI |
| Devices | Local adb devices and emulators | Marathon-hosted virtual devices | Firebase Test Lab only | CI-runner emulators the company runs |
| Maestro | **No** | **Yes** | **No** | **Yes** (native; v1 is Maestro-only, research 09 §7) |
| Espresso / instrumentation | Yes | Not checked | Yes (via FTL) | No (ADR-0011) |
| XCUITest / XCTest | Yes | Simulator builds | Yes (via FTL) | No in v1 |
| Appium | Not found | Not found | Not found | No |
| Cross-job split | md5 hash mod count (`fragmentation`) | Hosted | Manual shards; uniform (not recommended) | LPT across CI jobs, static manifest |
| Uses timings | Yes, in-run, from analytics backend | Yes (hosted history) | Yes, JUnit file in GCS | Yes, per-flow p50, p90 tie-break |
| Keeps Maestro sequences together | N/A | **No** (not supported) | N/A | **Yes** (`executionOrder`) |
| Preventive retries | Yes (probability model) | Yes | No | No (no automatic reruns) |
| Post-factum retries | Yes (quotas) | Yes (3 strategies) | Yes (FTL re-attempts, max 10) | No (spec §17) |
| Flaky label | Yes | Yes | Only in JUnit option | Yes (confirmed and suspected, ADR-0009) |
| Quarantine | No | No | No | No (ADR-0009) |
| Reports | JUnit, HTML, timeline, Allure | Cloud console | HTML, JUnit | Live dashboard, evidence viewer |
| Video / screenshots | Yes | Not checked | Video only | Not uploaded (research 09 §4.1) |
| Live progress | Terminal | Console | Terminal, FTL console | Live (ADR-0009) |
| History store | InfluxDB v1/v2, Graphite | Hosted | GCS JUnit file | Fleet's Postgres |
| Latest release | 0.10.5, 2026-07-21 | n/a | v23.10.1, 2023-10-26 | v0.1.0-alpha.2 (commit `9fde28c`) |
| Last commit | 2026-09-21 | n/a | 2026-02-25 | (not checked) |
| License | GPL-2.0 | Not stated | Apache-2.0 | AGPL-3.0-only (AGENTS.md) |
| Stars | 646 | n/a | 701 | n/a |
| Cost | Open source, cost not stated | $2/h Android, $3/h iOS | FTL: $1/h virtual, $5/h physical | Internal |

---

## 5. What it means for Fleet

### 5.1 Overlap with Fleet Planner and history features

**Where they overlap [I]:**

- Marathon's in-run execution-time sort and Fleet Planner's LPT both schedule long units first from history. Marathon does this per test, dynamically, across devices in one run. Fleet Planner does it once, statically, across CI jobs (ADR-0013). They work at different levels.
- Marathon's preventive retries and Fleet's flake method both read run history. Marathon adds reruns, which the spec excludes (spec §17 "Automatic failure retries").

**What Fleet Planner does that they do not [V]:**

- Keeps each `executionOrder` sequence in one shard (`crates/fleet-core/src/plan.rs:9`). Marathon Cloud does not support sequential flow logic (`docs/cloud/intro/maestro.md:12`). Flank has no Maestro support.
- Writes a GitHub Actions matrix across CI jobs (`crates/fleet-planner/src/main.rs:215`). Marathon's cross-job filter is a hash with no history; Flank's manual and uniform shards are per Firebase matrix.
- Writes an immutable manifest and publishes it, then compares planned with actual (ADR-0013).
- Declares mutually exclusive groups (ADR-0013). Neither tool has an equivalent.

**What they do that Planner does not [V]:**

- Marathon: device pools by model, OS and ABI; batching; probabilistic preventive retries; per-test device reroutes.
- Flank: uniform and manual sharding; Firebase's own device matrix.
- Planner has no retry and no batching, which follows ADR-0013 and spec §17.

**History design lessons [I]:**

- Flank's history is a merged JUnit file, and its own design doc calls the merge "complicated" (`docs/smart_flank.md`). Fleet's per-flow p50 and p90 from its own store avoids that merge.
- Marathon keys history per test through `MetricsProvider`. Fleet keys it per Maestro flow plus condition fingerprint. Those keys do not match, so importing Marathon history into Fleet would need a mapping.

### 5.2 Integrate or stay separate

Options only. Not decided here.

1. **Stay separate.** Consistent with ADR-0013 and the spec. No new dependency or data contract.
2. **Import Marathon's analytics store as a history source.** Needs a per-test-to-flow mapping. Marathon's data is per test, so it would be a weak substitute for Fleet's flow history.
3. **Observe Marathon Cloud Maestro runs.** Would need Marathon's output format, which the Cloud docs do not describe in the pages read. Cloud results would also leave the company, the same issue research 07 §2.4 raises for hosted products.
4. **Drive Marathon Cloud from a Fleet plan.** Fleet's shard list could feed Cloud's `--filter-file` per shard. This creates two schedulers; ADR-0013's model is that CI launches the jobs and Fleet only computes the assignment.
5. **Observe an open-source Marathon run.** Out of scope under ADR-0011, since v1 is Maestro-only.

**Licence [I]:** Marathon is GPL-2.0, Fleet is AGPL-3.0-only. Combining code from the two would need a licence review. Reading output files is a different question. This is not legal advice.

### 5.3 Maestro support as a positioning fact

- **Marathon Cloud runs Maestro (direct competitor, hosted only).** It sits with Maestro Cloud, BrowserStack and devicecloud.dev in research 09 §3: vendor-run devices, no self-hosted observability.
- **Marathon's open-source runner does not run Maestro.** If that changes, it would be a direct competitor for self-hosted runs. Nothing in the repo points that way **[I]**.
- **Flank does not run Maestro** and is tied to Firebase Test Lab. It is not a Maestro competitor.

Fleet's positioning is unchanged by this research: the gap is still live Maestro observability on the company's own runners.

---

## 6. Not verified

- Marathon Cloud pricing, retries and setup pages, and Firebase pricing, were read through a page-summarising fetch. Numbers are not checked against the raw HTML.
- `marathonlabs.github.io` and `flank.github.io` could not be fetched. The repo `docs/` sources were used instead.
- Marathon's open-source flaky classification, logcat collector, and `count` sharding semantics were not traced.
- Marathon Cloud's Android and iOS device details and Emu were not researched.
- Cause of Flank's `pushed_at` (2026-06-20) against its last default-branch commit (2026-02-25) was not identified.
- No live runs of either tool were made.
- Fleet's own last-commit date was not checked.

---

## Sources

**Repositories (read at HEAD, shallow clones)**
- Marathon: https://github.com/MarathonLabs/marathon (HEAD `34e5d3d0e6`, 2026-09-21)
- Flank: https://github.com/Flank/flank (HEAD `38d9c86b9f`, 2026-02-25)

**GitHub API (read-only, 2026-11-07)**
- https://api.github.com/repos/MarathonLabs/marathon and `/releases/latest`
- https://api.github.com/repos/Flank/flank and `/releases/latest`

**Marathon**
- Open-source docs (repo): `docs/runner/` in the Marathon repository
- Cloud Maestro docs (repo): `docs/cloud/intro/maestro.md`
- Cloud pricing (fetch): https://docs.marathonlabs.io/cloud/account/pricing/
- Cloud retries (fetch): https://docs.marathonlabs.io/cloud/configuration/retries/
- Cloud Maestro setup (fetch): https://docs.marathonlabs.io/cloud/maestro/setup/
- Cloud overview (fetch): https://docs.marathonlabs.io/

**Flank**
- Repo docs: `docs/index.md`, `docs/smart_flank.md`, `docs/test_sharding.md`, `docs/flank_vision.md`, `docs/hypershard_android.md`, `release_notes.md`, `README.md` in the Flank repository
- Documentation site (not fetchable): https://flank.github.io/flank/

**Firebase Test Lab**
- Pricing (fetch): https://firebase.google.com/docs/test-lab/usage-quotas-pricing
- Overview (fetch): https://firebase.google.com/docs/test-lab/overview

**Fleet (repo)**
- `docs/decisions/0013-fleet-planner.md`, `docs/decisions/0009-scope-focused-build.md`
- `research/07-prior-art-and-build-vs-buy.md`, `research/09-competitive-review-2026-11.md`
- `crates/fleet-planner/src/main.rs`, `crates/fleet-core/src/plan.rs`
- `docs/spec-v1.0.md` §17, `AGENTS.md`
