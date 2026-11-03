# 07: Prior art, build-vs-buy, and lessons from existing tools

**Research date:** 2026-11-03
**Scope:** Maestro's own products; test-results platforms; mobile device farms; generic CI observability (OpenTelemetry); Maestro community tooling; flakiness methods; failure fingerprinting.
**Spec reference:** `docs/spec-v1.0.md`. The spec suggests Python and TypeScript. The team plans to build all of it in Rust.

## How to read this report

Every claim carries a confidence marker:

- **[V]**: Verified on 2026-11-03 against a primary source: vendor docs, a pricing page, a source file or a paper. The URL is in the Sources section.
- **[V3]**: Verified only from a third-party source, such as a review site, a comparison page or a community blog.
- **[I]**: Inferred. This is my reasoning, prior knowledge I could not re-check today, or an extrapolation. Treat it as a hypothesis to test.

Prices are list prices shown on public pages on the research date. They change often, so confirm them before you budget.

---

## 1. Executive summary

1. **Nothing on the market does what the spec's core does, for this setup.** The spec's core is live, flow- and step-level Maestro progress from emulators on company-run CI runners, tied to emulator and host health, with explicit provenance. Every product with a live Maestro view runs the devices itself: Maestro Cloud, BrowserStack, TestingBot and devicecloud.dev. Every self-hosted results platform is framework-generic and knows nothing about emulators, hosts or Maestro: ReportPortal and Allure. **[V] for the product facts, [I] for "nothing does it": I searched and found no counter-example.**
2. **Most of the history and analytics half of the spec is commodity.** JUnit history, pass-rate and duration trends, flaky ranking, regex or ML failure grouping, run comparison and artifact attachments all exist in free, self-hosted, Apache-2.0 tools (ReportPortal; Allure Report 3 with Allure Report Storage) and in many SaaS products. **[V]**
3. **But "buy" for the commodity half has real costs in this context:**
   - ReportPortal is a stack of about 12 containers, including OpenSearch, RabbitMQ and Traefik. It needs 4 CPU, 8 GB and 300 GB of disk at its smallest size. **[V]** Its SSO (SAML) and LDAP support are moving to a paid subscription (announced April 2026), which matters because the spec requires company SSO. **[V]**
   - Allure's free tooling has no multi-user server with RBAC or SSO that I could verify. **[I]**
   - The SaaS products send screenshots and logs off-site, which the spec's security section treats as sensitive. **[V]** for the spec.
   - Splitting the UI across two products breaks the spec's main promise: one place where a metric spike, a device disconnect and a failed step line up on one timeline (Screens C and D).
4. **Recommended verdict: a focused build.**
   - **Build** the Fleet Agent, the live ingestion and state layer, the Maestro-aware data model (flows, steps, attempts, shards, provenance), the device and host health correlation, and a lean evidence viewer. Nothing else does these.
   - **Build thin** history, flakiness and grouping. The data is already in Fleet's Postgres, and the explainable methods below take days, not months.
   - **Do not build** ML auto-analysis, quality-gate engines, test-case management, fleet-wide capacity dashboards (use Grafana or OTel), or a general metrics time-series store.
   - **Make Fleet a good citizen:** use OTel `cicd.*` and `test.*` names in the event schema, add an optional OTLP exporter in the agent, and add optional exporters to Allure results and ReportPortal for teams that already standardise on them.
5. **If the team cannot fund the full spec**, the fallback is "build the agent plus the live view, and push final results to ReportPortal (or Allure 3)". This cuts Screens E and F and most of Phase 4, at the cost of a second UI and RP's operational weight. Section 6 compares the options.
6. **Flakiness v1:** a deterministic, explainable two-signal method, scoped to a "comparable conditions key":
   - **Confirmed:** pass and fail on the same commit under the same conditions.
   - **Suspected:** flip rate on the default branch.
   - Failures classified as infrastructure, or that coincide with host or device health anomalies, are excluded.
   - A flaky label never hides a failure. Section 7 has the details.
7. **Failure fingerprinting v1:** use Maestro's own exception taxonomy (`MaestroException.*`, verified in source), the failing step's identity (command, label, selector) and Sentry-style message parameterization, with a versioned grouping config and user-defined, first-match-wins rules. Section 8 has the details.

---

## 2. Maestro's own offerings (current state)

### 2.1 Naming and product line (as of 2026-11-03)

- **Maestro CLI:** open source, Apache-2.0, current release **CLI 2.12.1, released 2 Nov** **[V]**. The year is not shown on the releases page, so 2026 is **[I]** from the release sequence.
- **Maestro Studio:** a desktop IDE for authoring and debugging, with device mirroring. It can launch cloud runs and "monitor live results from the editor". **[V]**
- **Maestro Cloud:** the hosted execution platform. Its console is at `app.maestro.dev`. **[V]**
  - **"Robin"** was an interim name for the hosted product during 2025. The docs URL `docs.maestro.dev/cloud/robin` now redirects to the Maestro Cloud docs. The docs and pricing page use "Maestro Cloud" or "Cloud" again, and the old "Maestro Cloud is now Robin" page returns 404. **[V]** for the redirect and the current naming; **[V3]** for the history.
- **Maestro Viewer** (since CLI 2.6.0): "a desktop visualizer for live hierarchy and commands". It embeds a simulator, emulator or device view in a browser or coding agent through Maestro MCP. It is an authoring and debugging aid, not a CI dashboard. **[V]**
- **Maestro MCP:** an MCP server with `run_on_cloud`, `describe_cloud_run` and related tools. **[V]**

### 2.2 Pricing (maestro.dev/pricing) [V]

| Plan | Price | Relevant contents |
|---|---|---|
| Local | Free, OSS | CLI, Studio, MCP, Viewer, "self-healing with local agents" |
| Cloud | **$250 / device / month** | Hosted Android, iOS and web devices; parallel runs billed by max concurrency; unlimited runs; "rich test reporting"; CI and PR integration; free trial |
| Enterprise Cloud | Custom | SSO, dedicated support, custom contracts, security review |

**Example (illustrative):** 10 concurrent emulators would cost about $30k per year **[I]** (simple arithmetic).

### 2.3 Reporting, history, flakiness and live views in Maestro Cloud

- **Per-run evidence:** step-by-step video playback, detailed logs, and "flake detection" on every run. **[V]** (marketing page). The detection algorithm is not documented.
- **Smart retries:** Cloud automatically retries a failed flow when **(a)** the previous run of the same flow (keyed by `name`, or by filename) succeeded on any branch, and **(b)** it used the exact same device (OS and model). Infrastructure-triggered retries move to a different machine. **[V]**
  - *Lesson for Fleet:* even Maestro scopes its flake heuristic to the same device profile. This matches the spec's "comparable execution conditions".
- **Notifications and integration:** Slack, email, GitHub PR checks and merge blocking, and webhooks. **[V]**
  - The webhook payload is a useful schema reference: upload, then flows, each with `status`, `failureReason`, start and end times, `tags` and `properties`, plus `platform`, `deviceModel`, `osVersion` and `deviceLocale`. **[V]**
- **JUnit report properties for Cloud runs:** `cloud.uploadId`, `cloud.url`, `cloud.runId` and `cloud.runUrl` deep-link CI test viewers to the Cloud console. **[V]**
- **Live status:** the GitHub Action exposes upload states `PENDING`, `PREPARING`, `INSTALLING`, `RUNNING`, `SUCCESS`, `ERROR`, `CANCELED`, `WARNING` and `STOPPED`, plus per-flow results when not in async mode. **[V]** The Studio docs say you can monitor live cloud results. **[V]**
- **History and analytics:** the public pages describe no history or trend dashboard. **[V]** that it is absent from the pages; whether the console has one is **[I]** (unknown).

### 2.4 Can it be self-hosted? What data leaves the company?

- **Self-hosted Maestro Cloud:** no evidence on the pricing, cloud or docs pages. **[V]** that it is absent; **[I]** that it does not exist.
- **Using Maestro Cloud** means uploading to Maestro's infrastructure **[V]** (upload model):
  - app binaries
  - flows
  - environment parameters, including secrets passed with `-e` (docs: "Manage secrets")
  - screenshots, video and logs, which stay in Maestro's console
  Static egress IPs are published so customers can allowlist test backends. **[V]** Data retention, residency and SOC 2 status were not found on public pages. **[I]** Ask the vendor.
- **Local CLI** **[V]**:
  - Analytics collection is on by default and is disabled with `MAESTRO_CLI_NO_ANALYTICS=true`.
  - The update check is disabled with `MAESTRO_DISABLE_UPDATE_CHECK`.
  - `--analyze` and AI commands are "routed directly through Maestro Cloud". They need a Maestro login and send logs, command metadata and screenshots to Maestro's infrastructure and its third-party AI providers.
  - Fleet should surface these settings in `maestro-fleet check` and let IT enforce them. It should never silently change Maestro's behaviour. **[I]**
- **Fit with the spec:** Maestro Cloud swaps the execution model (hosted devices instead of the company's CI emulators) and moves the data off-site. It cannot meet "Self-hosted, infrastructure-independent". Its value for Fleet is as **prior art and a schema reference**, not as a replacement. **[I]**

### 2.5 Maestro features Fleet should build on (verified, high value)

These are prior art that Fleet *consumes*. Phase 0 must confirm each one against real runs.

- **Per-flow artifact bundle (CLI ≥ 2.7.0):** each flow folder holds:
  - `manifest.json`, "the documented contract, with a `$schema`"
  - `commands.json`: one entry per executed step, with command, `status`, `duration`, `sequenceNumber`, error and the list of produced artifacts. A retried step appears once per attempt.
  - `logs/maestro.log`, `device-logcat.txt` (Android), `crash-report.txt` and `anr-report.txt`
  - `screenshots/` (failing step only, unless `--analyze`), `screen-hierarchy/` (view hierarchy JSON on failing steps), `takeScreenshot/` and `startRecording/`
  - Since 2.11.0, the manifest records when a screen recording actually started, so video and commands can be aligned.
  - Sharded sessions add a `-shard-N` folder suffix. **[V]**
  - The docs **do not** say whether `commands.json` is written incrementally, so don't count on it for live progress until Phase 0 proves it. **[V]** (GitBook docs Q&A answer).
- **JUnit identity controls:** the flow header `properties` become `<property>` elements. The reserved `junitId` and `junitClassname` keys override the `<testcase id>` and `classname`. A `file` attribute exists since 2.6.0. **[V]** Recommend `junitId` as the stable flow identity where teams set it, falling back to the repository-relative path. **[I]**
- **Report formats:** `junit`, `html` and `html-detailed`. JUnit timestamps are truncated to whole seconds, and the suite `time` is wall-clock. **[V]**
- **Console format control:** `MAESTRO_CLI_LOG_PATTERN_CONSOLE` (a Logback pattern, default `%highlight([%5level]) %msg%n`) and `--no-ansi`. **[V]** The agent might set a stable, timestamped pattern to make live parsing more robust. This is **[I]**: check in Phase 0 that it does not change the user-visible output in ways they rely on.
- **Exception taxonomy** (source `maestro-client/.../Errors.kt`) **[V]**:
  - `MaestroException` subclasses: `UnableToLaunchApp`, `AppCrash`, `AppNotRunning`, `DriverTimeout`, `AssertionFailure`, `ElementNotFound` (a subclass of `AssertionFailure`), `InvalidCommand`, `InvalidNumericFieldValue`, `HideKeyboardFailure`, `WebViewInspectionFailure`, `UnableToCopyTextFromElement`, `DestinationIsNotWritable`, `CloudApiKeyNotAvailable`, `NoRootAccess`, `UnsupportedJavaVersion`, `MissingAppleTeamId` and `IOSDeviceDriverSetupException`.
  - Plus `MaestroDriverStartupException.{AndroidDriverTimeoutException, AndroidInstrumentationSetupFailure}`.
  - Section 8 maps these onto the spec's categories.

---

## 3. Test-results platforms

### 3.1 Comparison table

Legend: ✅ yes · ⚠️ partial or with caveats · ❌ no · ? unknown. The marker after each cell gives its confidence.

| Tool | Self-hosted | Live (during run) | Mobile / emulator-aware | Host metrics | Flakiness detection | Maestro ingestion | License / cost class |
|---|---|---|---|---|---|---|---|
| **Maestro Cloud** | ❌ [V] | ✅ console and Studio live status [V] | ✅ hosted emulators and simulators [V] | ❌ [I] | ✅ "flake detection" plus smart retries on the same device [V] | Native | SaaS, $250/device/mo; Enterprise custom [V] |
| **ReportPortal** | ✅ Docker or K8s [V] | ✅ streaming API: items start and finish in real time [V] | ❌ generic [I] | ❌ [I] | ⚠️ "Flaky test cases" widget (status switches over N launches); ML auto-analysis of logs [V] | No Maestro agent found [I]; JUnit import plugin [V]; reporting API [V] | Apache-2.0 free; **SSO/LDAP moving to paid** [V]; SaaS $569 to $2,659/mo [V] |
| **Allure Report 3** (+ Allure Report Storage) | ✅ static report; Storage service in Docker [V] | ⚠️ `allure watch`: local, single machine [V] | ❌ [I] | ❌ [I] | ✅ rule-based "Flaky", "New failed" and similar marks [V] | Via JUnit XML [V3] or an allure-results writer [I] | Apache-2.0 (Storage stated Apache-2.0 [V]) |
| **Allure TestOps** | ✅ on-prem [V] | ⚠️ real-time result collection claimed [V3] | ❌ [I] | ❌ | ⚠️ yes per third parties [V3]; regex "defect" automation rules [V] | Via JUnit or Allure results [I] | $39/user/mo, cloud or on-prem [V] |
| **Currents.dev** | ⚠️ Enterprise only [V] | ✅ [V] | ❌ | ❌ | ✅ [V] | ⚠️ "JUnit-based runners" supported [V]; Maestro via JUnit [I] | SaaS, $49/mo per 10k results and up [V] |
| **Cypress Cloud** | ❌ [V] | ✅ (Cypress only) [I] | ❌ | ❌ | ✅ Team plan and up [V] | ❌ Cypress only | SaaS, $67/mo and up [V] |
| **Sorry-Cypress** | ✅ [V] | ✅ (Cypress) [I] | ❌ | ❌ | ❌ [I] | ❌; also blocked by Cypress ≥ 13 [V3] | MIT [V]; effectively stranded |
| **Buildkite Test Engine** | ❌ [I] | ⚠️ ingestion per build [I] | ❌ (an Android collector exists for JVM tests [V]) | ❌ (Pipelines has agent metrics) [I] | ✅ monitors: transition count, passed-on-retry, Meta PFS, new test, duration threshold [V] | JUnit XML import [V] | SaaS; free 250k exec/mo without workflows; Pro 1M exec plus $15/M; history window 24 h, 7 d or 28 d by tier [V] |
| **Datadog Test Optimization** | ❌ [V] | ✅ spans as they finish (tracer) [I] | ⚠️ Swift is native; others via JUnit [V] | ⚠️ only with the Datadog Agent and Infrastructure product [I] | ✅ but Early Flake Detection and Auto Test Retries **not** for JUnit uploads [V] | JUnit upload via `datadog-ci` [V] | $20/committer/mo annual, $29 on-demand; 1M spans per committer [V] |
| **Trunk Flaky Tests** | ⚠️ Enterprise, "private preview" [V] | ❌ processed on upload; up to 20 min to flag [V] | ❌ | ❌ | ✅ pass-on-retry, failure rate and failure count monitors; Healthy, Flaky or Broken [V] | JUnit upload [I] | Free for ≤5 committers; Team seat price not published; $3 per extra 1M spans [V] |
| **CloudBees Smart Tests** (ex-Launchable) | ❌ [I] | ❌ | ❌ | ❌ | ✅ daily flaky analysis [V] | JUnit via CLI [I] | Quote-based; third-party sites say about $12k/yr [V3] |
| **BuildPulse** | ❌ (not mentioned) [V] | ❌ | ❌ | ❌ | ✅ detection and quarantine on all plans; method undocumented [V] | JUnit [I] | $99 to $499/mo [V] |
| **Testmo** | ❌ "no on-premise edition" [V] | ❌ [I] | ❌ | ❌ | ? not mentioned [V] | JUnit via CLI [I] | $59 to $599/mo [V] |
| **TestRail** | ✅ Server, ≥10 seats, 12-month minimum [V] | ❌ | ❌ | ❌ | ❌ [I] | `trcli` JUnit [V] | €38 or €78/user/mo [V] |
| **Tesults** | ❌ none found [V3] | ? | ❌ | ❌ | ✅ claimed [V3] | JUnit XML [V3] | From about $19/user/mo [V3] |
| **Gradle Develocity** | ✅ self-hosted install [V3] | ⚠️ build scans [I] | ❌ | ⚠️ build resource usage [I] | ✅ FLAKY means fail plus pass within one task or goal execution [V] | ❌ Gradle, Maven, Bazel and sbt builds only [V] | Commercial, quote [I] |
| **dorny/test-reporter** | n/a (runs in CI) | ❌ | ❌ | ❌ | ❌ | `java-junit` (experimental) [V] | MIT [V] |
| **mikepenz/action-junit-report** | n/a | ❌ | ❌ | ❌ | ⚠️ `check_retries`, `flaky_summary` [V] | JUnit [V] | Apache-2.0 [V] |
| **EnricoMi/publish-unit-test-result-action** | n/a | ❌ | ❌ | ❌ | ❌ [I] | JUnit and others [V3] | Apache-2.0 [I] |
| **Expo EAS Insights** (Maestro tab) | ❌ | ❌ [I] | ✅ through EAS Workflows [V] | ❌ | ✅ "passed only after retries"; flake rate and P90 per flow [V] | Native, JUnit from the EAS `maestro` job [V] | Expo Production or Enterprise plan [V] |
| **Flakiness.io** | ⚠️ optional self-host (Postgres + S3) [V3] | ? | ❌ | ❌ | ✅ flip rate [V] | JUnit XML [V3] | Freemium, priced by storage; not OSS [V3] |
| **OTel Collector + Grafana stack** | ✅ | ✅ metrics and traces | ❌ | ✅ hostmetrics receiver [V] | ❌ | ❌ | Apache-2.0 (Grafana is AGPL) [I] |
| **Fleet (as specified)** | ✅ | ✅ flow and step level | ✅ emulator, ADB, boot, AVD | ✅ host and process, thermal "unavailable" | ✅ (Phase 6) | Native | Internal build |

### 3.2 Notes per tool that matter for the decision

**ReportPortal (RP)** is the closest open-source match.

- *Architecture* **[V]**: the master `docker-compose.yml` runs `gateway` (Traefik), `postgres` 18.6, `rabbitmq` 4.4, `opensearch` 3.9, `migrations`, `index`, `ui`, `api`, `uat` (authorization), `jobs` and `service-auto-analyzer`. Component images are tagged 5.15.x. The docs site is versioned 26.0.7, so RP appears to have moved to calendar versioning **[I]**.
- *Hardware* **[V]**: Small is 4 CPU, 8 GB and 300 GB. Middle is 8 CPU, 16 GB and 500 GB. The K8s Small estimate is about $544 a month (Frankfurt).
- *Reporting model* **[V]**:
  - A Launch contains test items of type suite, test, step and hooks. Child items nest under a parent UUID.
  - **Nested steps** use `hasStats:false`, so they don't count as tests. This is exactly the spec's "a nested `runFlow` is a step, not a test".
  - Items have `attributes` (key and value), a `retry` flag, and launch `rerun` and `rerunOf`.
  - Statuses are `passed`, `failed`, `stopped`, `skipped`, `interrupted` and `cancelled`.
  - Logs can carry multipart attachments.
  - Issue types are `To Investigate` (the default for failures), Product Bug, Automation Bug, System Issue and No Defect.
  - An async API is available (`v2` endpoint).
- *Analysis* **[V]**:
  - Auto-Analysis uses OpenSearch TF-IDF similarity over error logs, then an XGBoost model with about 30 features. It assigns a defect type when the probability is ≥ 50%, trained on prior human triage.
  - "Unique Error Analysis" groups unique error logs, with a setting to exclude numbers.
  - The Flaky widget counts Passed↔Failed switches over 2 to 100 previous launches (default 30), using only the last retry in each launch.
- *Licensing shift* **[V]**:
  - The 2026-04-02 post says all SSO (SAML) and LDAP move to the paid subscription, multiple Organizations are premium, and Active Directory is dropped from 26.1.
  - GitHub auth stays free. Quality Gates are a premium feature.
  - **For the spec's OIDC SSO requirement**, self-hosted RP probably needs a paid package or an auth proxy in front of it **[I]**. OIDC specifically was not confirmed either way.
- *Maestro:* no Maestro agent or integration found **[I]** (searched). The paths would be the JUnit import plugin (`POST /v1/plugin/{project}/junit/import`), which loses steps and screenshots, or a custom reporter calling the API.

**Allure Report 3** is the lightweight open-source option.

- *Test identity* is `testCaseId`, or `fullName` plus non-excluded parameters, plus the **environment** where environments are configured. `historyId` is no longer used. **[V]**
- *History* is stored in one JSONL file (`historyPath`). The new **Allure Report Storage** service (self-hosted, Docker or Cloudflare Worker, Apache-2.0) keeps history by repository and branch, and publishes completed reports. It stores them on the filesystem, S3 or R2. **[V]**
- *Flaky rule (Allure 3)* **[V]**:
  - Only a test that is Failed or Broken in the latest launch can be marked.
  - It looks at up to 10 previous results in the same environment and ignores Skipped.
  - Five identical statuses in a row reset the history.
  - One change, or one "recovery", is not enough. Two or more changes are.
  - Other marks: "New failed", "New passed", "New broken" and "Changed on retry".
- *Live:* `allure watch` gives local real-time updates **[V]**. It is not a multi-runner server.
- *Categories* are regex rules on message and trace, with `groupByMessage` **[V]**.

**Buildkite Test Engine** has the best-documented flaky monitors in industry. **[V]**

- *Transition count* gives a score over a fixed window, always divided by the window size:
  - FFFFF = 0; PPFFF = 0.2; PFPFF = 0.4.
  - PF in a window of 5 = 0.2.
  - It needs a branch filter, usually `main`, because "failures and passes on feature branches are a byproduct of a standard development workflow".
- *Passed on retry* means at least one pass and one fail on the same commit SHA. It recovers after 7 days or 100 executions without recurrence.
- *Probabilistic flakiness* is Meta's PFS.
- *Duration threshold* compares the p50 over a window of up to 100 runs with alarm and recover thresholds.
- *Tag filters* support up to 4 per workflow; the default branch filter is `scm.branch`.

**Datadog Test Optimization** **[V]**:

- A test is flaky only if it is inconsistent **within the same configuration**. The example is one Python version failing and then passing on retry while the others pass.
- Environment tags (OS, architecture, `test.configuration.*`) define the configuration.
- Non-deterministic parameters create "new tests", which breaks history.
- This is the clearest industry statement of the spec's "comparable conditions" rule.

**Trunk Flaky Tests** **[V]**:

- Monitors run independently. "Most severe wins" across Broken, Flaky and Healthy.
- Mutes expire automatically (1 hour to 30 days).
- Failure Rate monitors are scoped by branch. Pass-on-Retry ignores branches.
- "Variants" separate environments.

**GitHub-native reporting** **[V]**:

- There is no first-party GitHub test-results product; I found no changelog entry. The ecosystem is Marketplace actions.
- `mikepenz/action-junit-report@v6` handles annotations, check runs, job summaries, `check_retries` and `flaky_summary`.
- `dorny/test-reporter@v3` handles check runs and job summaries. Its `java-junit` support is experimental, it is limited to 65,535 bytes, and it has no screenshot links.
- These are the **baseline Fleet must beat**. Teams already get pass and fail per job in the PR for free, so Fleet's value has to be live progress, evidence and history.

---

## 4. Mobile device farms and emulator observability

| Product | Live device state | Per-test evidence | Maestro? | Self-hosted |
|---|---|---|---|---|
| **DeviceFarmer STF** | ✅ connected, offline, unauthorized and unplugged states; battery level and health; live screen at 30 to 40 FPS; log filtering [V] | Device logs; not test-oriented [I] | ❌ no Maestro integration (devices are reachable over `adb connect`) [I] | ✅ Node.js ≤ 20, RethinkDB, ZeroMQ; "slow" volunteer development; not hardened for multi-user [V] |
| **GADS** (shamanec) | ✅ live MJPEG or WebRTC streaming and remote control; reservations [V] | Logs in MongoDB [V] | ❌ not mentioned; Appium or Selenium Grid oriented [V] | ✅ Hub and Provider plus MongoDB; AGPL-3.0, with a **proprietary, obfuscated hub UI** [V] |
| **Firebase Test Lab** | Hosted | Video, logs, performance per matrix cell [I] | ❌ instrumentation, Robo, Game Loop and XCTest only [V3] | ❌ |
| **emulator.wtf** | Hosted emulators | ? | ❌ not confirmed [I] | ❌ |
| **Genymotion SaaS** | Instance dashboard [I] | ❌ | ⚠️ Maestro runs over `gmsaas instances adbconnect` (community guide); not in the official framework list [V3] | ❌ SaaS (on-prem images exist [I]) |
| **BrowserStack App Automate** | Dashboard per build [I] | ✅ video for every test, Maestro debug output and per-flow command logs, REST API [V] | ✅ native Maestro (reported as Beta) [V]/[V3] | ❌ |
| **TestMu AI (ex-LambdaTest) HyperExecute** | HyperExecute dashboard [V3] | ? | ✅ Maestro docs exist [V3] | ❌ |
| **Sauce Labs** | — | — | ⚠️ "community framework": DeviceLab's `maestro-runner` translates flows to Appium [V] | ❌ |
| **TestingBot** | ✅ live video and step streaming [V] (vendor) | Video, screenshots, logs [V] | ✅ native [V] | ❌ |
| **devicecloud.dev** | ? | ? | ✅ drop-in `dcd cloud`; 6-month retention on Pro; from $25/mo plus credits [V] | ❌ |

**Lessons:**

- **The live-device UI pattern is settled.** It is a device grid with a state badge and a freshness indicator, and a drill-down that shows the current test, a log tail and a screenshot. Fleet's spec deliberately excludes live screen streaming and remote control, which is the expensive part of STF and GADS. That is a good call: those features are what make STF and GADS heavy, insecure by default, or license-encumbered. **[I]**
- **Per-test evidence norm:** video for each test, a command log tied to steps, a device log, and a screenshot on failure. Maestro's own artifact bundle already produces all four locally (§2.5). Fleet's job is to collect, index and present them, not to generate them. **[V]** for the bundle.
- **None of these ingest Maestro from *your* CI runners.** Every Maestro-aware product executes Maestro itself. **[V]** for each listed product; **[I]** for the generalisation.
- **Device state vocabulary to reuse:** STF's adb states (device, offline, unauthorized, unplugged) map directly onto the spec's "ADB connection state". Add `booting` and `boot_completed`, taken from `sys.boot_completed`. **[I]**

---

## 5. Generic CI observability: OpenTelemetry, otel-cli, githubreceiver, Prometheus

### 5.1 Status (OTel semantic conventions 1.44.0) [V]

- **CI/CD conventions: "Release Candidate"** (spans and the overall CI/CD page). The CI/CD metrics page is "Mixed".
  - Attributes (all RC): `cicd.pipeline.name`, `cicd.pipeline.run.id`, `cicd.pipeline.run.url.full`, `cicd.pipeline.run.state` (pending, executing or finalizing), `cicd.pipeline.result` (success, failure, error, timeout, cancellation or skip), `cicd.pipeline.task.name`, `cicd.pipeline.task.run.id`, `cicd.pipeline.task.run.result`, `cicd.pipeline.task.run.url.full`, `cicd.pipeline.task.type`, `cicd.pipeline.action.name`, `cicd.system.component`, `cicd.worker.id`, `cicd.worker.name`, `cicd.worker.state` (available, busy or offline) and `cicd.worker.url.full`.
  - Metrics: `cicd.pipeline.run.duration`, `cicd.pipeline.run.active`, `cicd.worker.count`, `cicd.pipeline.run.errors` and `cicd.system.errors`.
  - The metrics page explicitly recommends **host metrics of the worker, associated with the pipeline run, "to correlate build failures with environment issues like overload or out-of-memory"**. The pipeline-run association must be opt-in because of cardinality. That is Fleet's thesis, written into the standard.
- **Test conventions: "Development"**:
  - `test.case.name`
  - `test.case.result.status` (pass or fail)
  - `test.suite.name`
  - `test.suite.run.status` (success, failure, skipped, aborted, timed_out or in_progress)

### 5.2 Tools [V]

- **githubreceiver** (Collector contrib) is **alpha** for metrics and traces.
  - It turns `workflow_run` and `workflow_job` webhooks into traces. Trace and span IDs are **deterministic**, so in-job tools can emit correlated spans.
  - It also scrapes VCS metrics.
  - It needs a publicly reachable webhook endpoint and cannot backfill **[V3]**.
- **otel-cli** (equinix-labs) is experimental and pre-1.0. It provides `exec`, `span background` and `TRACEPARENT` propagation.
- **hostmetrics receiver** is beta. It has cpu, memory, load, paging, processes, process and system scrapers, and **no thermal metric**.
- **opentelemetry-rust**: metrics and logs (API, SDK and OTLP exporter) are **Stable**; traces are **Beta**.

### 5.3 Can Fleet use OTel instead of building metrics storage?

| Option | Verdict |
|---|---|
| **Replace** Fleet's live transport with OTLP | ❌ Not recommended. OTLP has no application-level resume, ack-by-sequence or authoritative-state snapshot. The spec's reliability model (§5: sequence, replay, idempotency, "do not invent success") needs Fleet's own protocol. Test conventions are still "Development". **[I]** |
| **Replace** Fleet's metrics store with Prometheus or Mimir | ⚠️ Possible but not worth it in v1. The spec needs about 5-second samples while a run is live and 1-minute downsampled history for 90 days, keyed to jobs, devices and flows. That is a small volume Postgres handles; the spec already says "no TSDB without demonstrated need". Joining TSDB data to flow timelines in the UI adds a second query path. **[I]** |
| **Emit** OTLP from the agent, as a complementary option | ✅ Recommended (Phase 5 or 6, behind a flag). The agent already samples host and process metrics; exporting them with `cicd.*` resource attributes to the company Collector costs little. Platform teams then get runner-capacity dashboards and alerts in Grafana. Fleet need not build "historical runner utilization" analytics beyond basics (spec Screen E and the "CI capacity" question). Optionally emit suite, flow and step spans, parented to the githubreceiver's deterministic job span, to put Maestro inside CI traces. **[I]** Confirm the receiver's ID derivation before relying on it. |
| **Ingest** OTLP into Fleet | ❌ Not v1. It adds an attack and support surface for no clear user. **[I]** |
| node_exporter or Prometheus scraping of runners | ❌ Conflicts with the spec ("no permanent daemon", "no inbound runner access"). On ephemeral runners, push is the only fit: OTLP or remote-write via a Collector. **[I]** |

**Concrete recommendation:** name Fleet's event and attribute fields after OTel where an equivalent exists: `cicd.pipeline.run.id`, `cicd.pipeline.task.run.id` (job), `cicd.worker.name`, `vcs.ref.head.revision`, `system.cpu.utilization` and `system.memory.usage`. Then OTLP export later is a mapping, not a redesign. Keep Fleet-specific concepts (shard, flow attempt, provenance) in a `maestro.*` or `fleet.*` namespace. **[I]**

---

## 6. Build-vs-buy assessment

### 6.1 Capability-by-capability

| Spec capability | Exists elsewhere for this context? | Classification | Recommendation |
|---|---|---|---|
| Agent wrapping `maestro test`, preserving the exit code, CI context, buffering and reconnect | No (Maestro-aware CI-side wrappers don't exist) [I] | **Differentiated** | **Build** (Rust) |
| Live flow and step progress from self-hosted runners | Only for vendors' own devices [V] | **Differentiated** | **Build** |
| Emulator and ADB state, boot readiness, AVD and API level per device | Device farms (own hardware) [V]; no CI-side tool [I] | **Differentiated** | **Build** |
| Host, process and thermal metrics correlated to a flow timeline | Datadog (separate products, SaaS) [I]; OTel standard recommends it [V] | **Differentiated** (correlation); **commodity** (raw collection) | **Build** the correlation; consider OTLP export for the raw metrics |
| Maestro-aware accounting (shard-all vs shard-split, `runFlow` nesting, command retry ≠ attempt, planned vs observed) | None [I] | **Differentiated** | **Build** |
| Provenance (observed, provisional, verified, inferred, stale) | None, as a first-class concept [I] | **Differentiated** | **Build** |
| Live ↔ JUnit reconciliation | None [I] | **Differentiated** | **Build** |
| Evidence viewer (screenshots, `commands.json` timeline, logcat excerpt, hierarchy) | RP or Allure attachments (generic) [V]; Maestro Cloud (hosted) [V] | Mostly differentiated (Maestro-bundle aware) | **Build lean**; sandbox HTML reports (spec §10) |
| JUnit history, pass-rate and duration trends, slowest and most-failing flows | RP, Allure, every SaaS [V] | **Commodity** | **Build thin** on Fleet's own tables (SQL views), or export |
| Flaky detection and ranking | RP, Allure, Buildkite, Trunk, Datadog [V] | **Commodity** method; **differentiated** conditioning on device and host health | **Build thin** (§7) |
| Deterministic failure classification and grouping | Allure categories, TestOps regex rules, RP UEA, Sentry [V] | Commodity technique; Maestro-specific rules | **Build thin** (§8) |
| ML auto-analysis | RP [V] | Commodity, heavy | **Don't build** |
| Run comparisons (Screen F) | Partial in RP and Allure [I] | Commodity-ish; the "explain the mismatch" part is differentiated | Build after Phase 4; keep simple |
| Quality gates, TMS, manual test cases | RP (premium), TestRail, Testmo [V] | Commodity | **Don't build** (spec non-goal) |
| Fleet-wide CI capacity dashboards | Grafana and OTel [I] | Commodity | Basic in Fleet; **delegate** deep analytics via OTLP |
| SSO (OIDC), RBAC, audit | Every enterprise product, often paid [V] | Commodity, but required | Build on a standard OIDC library or reverse proxy. Note that buying RP does not remove this cost (SSO is paid) |

### 6.2 Options

| Option | What it is | Pros | Cons | Verdict |
|---|---|---|---|---|
| **A. Full custom build** (spec as written) | Agent, server, web, history and analytics | One correlated UI; matches every acceptance criterion; data stays internal | Largest effort: Phases 0–5 likely need several engineer-quarters [I]; re-implements commodity analytics | Justified **only if** history and analytics stay thin (A′) |
| **A′. Focused build (recommended)** | A, but history, flaky and grouping use the simple explainable methods in §7 and §8; no ML, TSDB, TMS or quality gates; optional OTLP, Allure and RP exporters | Keeps the correlated UX; analytics cost drops to SQL plus a few hundred lines of rules [I]; exits to standard tools | Still owns SSO, RBAC and retention, and the team is on call for it | ✅ **Recommended** |
| **B. Hybrid: build agent and live view, buy history** | Fleet agent plus a small live server and UI; final results and attachments pushed to **ReportPortal** (reporting API: nested steps, attributes, attachments, retry flag) or **Allure 3 + Report Storage** | Less to build (Screens E and F, most of Phase 4); mature trend widgets | Two UIs and two auth systems; RP is about 12 containers incl. OpenSearch and RabbitMQ; RP SSO is paid; Allure has no verified RBAC; the host↔flow correlation lives only in Fleet; investigation jumps between tools | ✅ **Fallback** if capacity is short; prototype in Phase 0 |
| **C. Buy hosted execution** (Maestro Cloud, BrowserStack, devicecloud.dev, TestingBot) | Move test execution to a vendor | Live view, video and flake handling out of the box | Changes the execution model (out of spec); data, including the app binary and secrets, leaves the company; $250/device/mo for Maestro Cloud [V]; runner and host health becomes the vendor's problem (and invisible) | ❌ for the stated requirements. Keep as a benchmark |
| **D. Do nothing new** | JUnit, `mikepenz/action-junit-report`, `upload-artifact`, optionally Allure | Near-zero cost | No live view; no device or host health; evidence lost with artifact retention; no cross-run history | Baseline only. Fleet must clearly beat it |
| **E. Generic SaaS test analytics** (Datadog, Buildkite, Trunk, Currents) | Upload JUnit | Good flaky tooling | Not live for JUnit; not Maestro or emulator aware; SaaS data egress; Datadog EFD and ATR don't work with JUnit [V] | ❌ for the core; possibly a complement if a team already pays |

### 6.3 Where a custom build is *not* justified

To be objective, these parts of the spec should **not** be custom-built in v1:

- **Statistical or ML flakiness models** (PFS, FlakeFlagger-style prediction). The explainable heuristic in §7 is enough at Fleet's scale. Even Buildkite reserves PFS for "large and complex test suites" and Enterprise. **[V]**
- **A metrics time-series store or capacity analytics** beyond simple charts. Use Postgres downsampling as the spec says, and leave fleet-wide capacity to OTLP and Grafana. **[I]**
- **A report generator.** Maestro already produces HTML and `html-detailed`. Fleet should store and sandbox-serve them, not regenerate them. **[V]** for the formats.
- **AI failure summaries.** These are already deferred in the spec. Note that Maestro's own `--analyze` sends data to Maestro Cloud. **[V]**
- **Notifications beyond a webhook and Slack.** Wait for demand. **[I]**

### 6.4 Suggested Phase 0 add-on (1–2 days)

Run the Phase 0 fixtures (success, failure, multi-flow, sharded) through **ReportPortal** (via JUnit import) and **Allure 3**, both from `docker compose`. Record exactly what is lost: steps, device, shard, screenshots, timing, live progress. This gives a concrete "what buying gets us" baseline for the go or no-go on Option A′ versus B, and it validates the export adapters early. **[I]**

---

## 7. Flakiness detection: methods and the recommended v1

### 7.1 What the literature and industry say

| Source | Key finding or method | Relevance |
|---|---|---|
| Google, "Flaky Tests at Google…" (2016) [V] | About 1.5% of all test runs are flaky; almost 16% of tests have some flakiness; **84% of pass→fail transitions involve a flaky test** | Most new failures are not regressions, so triage tooling must separate the two |
| Google, "Where do our flaky tests come from?" (2017) [V] | Flakiness grows with test size and **RAM use**. **Android emulator tests: 25.46% flaky vs 1.65% for all tests**; WebDriver tests are also high | Strong empirical support for correlating failures with host memory pressure and emulator health (the spec's thesis) |
| Meta, "Probabilistic Flakiness" (2020) [V] | Bayesian model in Stan with p_b (probability of a bad state) and p_f (fail probability in a good state, = PFS). Uses existing CI results plus a retry policy and "no extra runs"; retries pinned to the same code and config version; scores trend per test; alerts go to owners | Continuous score with uncertainty. Over-engineered for v1, but the **"same version of code and state of the world"** definition is the right one |
| Apple, Kowalczyk et al., ICSE-SEIP 2020 [V] | **Entropy** (randomness) plus **flipRate** (temporal variation), aggregated over time; 44% flakiness reduction with < 1% loss of fault detection | Flip rate is simple and explainable |
| Flakiness.io docs [V] | Flip rate = flips / (invocations − 1) over commit-level statuses; a "flaked" commit counts as fail then pass | A concrete formula to copy |
| Spotify (2019) [V] | **Odeneye** grid: tests × CI runs. "Scattering of orange dots" means flakiness; "**solid column**" means infrastructure. Flakybot reruns tests on a PR | Borrow the visualization and the column heuristic for infra events |
| FlakeFlagger, ICSE 2021 [V] | Predicts flakiness from features (execution time, coverage, changed-line coverage, third-party libraries). Even 10,000 reruns miss some flaky tests | Prediction needs coverage data Fleet doesn't have. Skip for v1 |
| Romano et al., UI-based flaky tests (2021) [V] | 235 UI flaky tests: **async wait is 45%** (32.5% on mobile); resource rendering, network loading | Expect timing and element-not-found failures to dominate. Classification must separate `ElementNotFound` timing from real assertions |
| Thorve et al., Android flaky tests, ICSME 2018 [V] | Concurrency is the top cause; dependency, program logic and UI causes are new for Android | Same message |
| Haben et al., Chromium CI (2023) [V] | Flaky-test predictors had 99.2% precision but **missed 76.2% of regression faults**; **flaky tests revealed more than a third of regression faults** | **Never auto-dismiss a failure because its test is "flaky".** Classify failures, not tests |
| Buildkite, Trunk, Allure, RP, Expo, Maestro Cloud [V] | Transition count over a window on the default branch; pass-on-retry on the same SHA; status-switch counts; "passed only after retries"; smart retry on the same device | Industry converges on two signals: **same-commit inconsistency** and **history flip rate**, scoped by branch and environment |

### 7.2 Recommended v1 method for Fleet ("explainable flakiness")

**Step 1: define the comparison cell (comparable execution conditions).** A *condition key* is:

```
condition_key = (
  flow_identity,          # normalized repo-relative path (or junitId) + platform
  device_profile,         # AVD name / device model + OS API level (+ ABI, locale if set)
  suite_config,           # suite name, include/exclude tags, shard mode (shard-all vs split)
  maestro_minor_version,  # e.g. 2.12 — CLI changes can change behavior
  app_variant             # build flavor/config if known from CI metadata
)
```

Branch is **not** part of the key. It is a *filter*: by default, only the default branch and merge-queue branches count for history signals. This follows Buildkite and Trunk, where feature-branch failures are expected. **[V]** for their practice; **[I]** for the key design. Show the key in the UI next to every score. Mixing cells is the main cause of misleading flaky numbers. This is the spec's §8 Screen F warning, applied to flakiness.

**Step 2: label failures, not just tests.** For each failed flow execution:

1. If the failure classification (§8) is infrastructure-class, label it **`infra-impacted`** and exclude it from test-flakiness signals. Infrastructure-class means `device_disconnected`, `emulator_boot_failure`, `infrastructure_failure`, `process_terminated`, or `installation_failure` when caused by ADB.
2. If it overlaps a recorded host or device anomaly, mark it **`environment-suspect`**, with the anomaly as evidence. Anomalies include memory pressure at or above "warn", a thermal state at or above "serious" where available, ADB state ≠ `device`, or swap above a threshold. Count these separately. Following Google's RAM and emulator data, they are likely flaky, but the cause is the environment.
3. **Column rule (Spotify):** if at least X% (default 50%, with N ≥ 4) of flows on the same device or worker fail within the same time window, flag a **device or worker incident**. Those failures become `environment-suspect`, not test flakiness.

**Step 3: compute two signals per condition cell.**

| Signal | Definition | Default threshold | Evidence shown |
|---|---|---|---|
| **Confirmed flaky** (same-commit inconsistency) | In one cell and one commit SHA: at least one `passed` and at least one `failed` (non-infra) flow execution. Sources: flow attempts from a CI job rerun, Maestro Cloud-style retries if the team uses them, `--shard-all` replicas on an *identical* device profile, or manual reruns. In-flow `retry` commands and command-level retries do **not** count (spec §4) | 1 event triggers; recover after 14 days or 50 executions with no event (Buildkite uses 7 days or 100) | The exact pass and fail executions, with links |
| **Suspected flaky** (flip rate) | Over the last N commit-level outcomes in the cell, filtered to the default branch. Collapse each commit to: pass; fail; or *flaked* (both, counted as fail→pass). Score = flips / (invocations − 1), where invocations count flaked commits twice (Apple and Flakiness.io) | N = 20; require ≥ 8 invocations; flag at ≥ 0.20; recover below 0.10 | A sparkline of outcomes; flips highlighted |

Also show these as information, not labels:

- **failure rate**, with a simple Beta(1,1) credible interval, so a 1-in-3 sample is not shown as "33%" without context
- **pass-after-retry rate**, which the spec §9 asks for

The labels follow Trunk's "most severe wins": **Broken** (consistent failure, flip score 0 and the last k ≥ 3 outcomes failing), then **Flaky (confirmed)**, then **Flaky (suspected)**, then **Healthy**.

**Step 4: presentation rules.**

- A flaky label never changes a failure's status, never hides it, and never affects the CI exit code (spec principle). Show "Failed — this flow has confirmed flaky history in this condition" (Haben et al.).
- Every score links to the runs that produced it, which is the spec's provenance principle.
- Treat a new flow or a new condition key as "insufficient history", not as healthy. Allure 3 and Buildkite have "new test" handling.

**Why not PFS or ML in v1:** they need more data than most single-company Maestro suites produce per condition cell, and they are harder to explain to a developer looking at one failure. Keep the schema compatible: store per-execution outcomes with condition keys and attempt numbers, so a Beta-Binomial or PFS-style score can be added later as Phase 6 work. **[I]**

### 7.3 Data the spec must capture for this to work

- A stable flow identity that is not the display name. The spec already requires this; prefer `junitId` when set. **[V]** that the key exists.
- An attempt number and its cause (CI job rerun, retry policy, manual), and the commit SHA from the CI adapter.
- The device profile per flow execution (AVD, API level), and the shard mode.
- Host and device anomaly intervals: from Fleet's own samples, with explicit availability per the spec.

---

## 8. Failure classification and grouping (fingerprinting)

### 8.1 Prior art [V]

- **Sentry:**
  - Grouping priority: custom **fingerprint**, then **stack trace** (in-app frames, normalized filename and context), then **exception** type and value, then **message**.
  - **Grouping config versions** apply only to new events.
  - Merge and unmerge.
  - Fingerprint rules use `matcher:expression -> values`, first match wins, with matchers such as `error.type`, `error.value`, `message`, `stack.function` and `tags.*`, and variables such as `{{ default }}`.
  - Message parameterization replaces, in order: email, url, hostname, traceparent, uuid, sha1, md5, date, duration, mac_addr, ip, swift_txn_id, hex, git_sha, random_id, float, int, quoted_str and bool. Each becomes `<name>`.
- **Allure:** "categories" are regex on message and trace mapped to named categories, with `groupByMessage`. **TestOps** "defects" use regex automation rules on message and trace, which also apply to future launches.
- **ReportPortal:** "Unique Error Analysis" groups errors with an option to ignore numbers; ML Auto-Analysis assigns defect types.

### 8.2 Recommended v1 design for Maestro failures

**Layer 1: deterministic category.** These are the spec's categories. Each records `rule_id`, `evidence[]` and `confidence`.

| Evidence (strongest first) | → Spec category | Confidence |
|---|---|---|
| Agent saw the emulator process exit, ADB state went `offline` or missing during the flow, or `MaestroDriverStartupException.*` | `device_disconnected` or `emulator_boot_failure` | High (observed) |
| `crash-report.txt` or `anr-report.txt` present for the flow, or `MaestroException.AppCrash`, or `AppNotRunning` | `application_crash` | High |
| `UnableToLaunchApp`; install or `adb install` errors in the log | `installation_failure` (if install) or `application_crash` | Medium |
| `ElementNotFound` | `element_not_found` | High (structured) |
| `AssertionFailure` (not `ElementNotFound`) | `assertion_failure` | High |
| `DriverTimeout`, or a step hitting its timeout | `timeout` | Medium |
| `InvalidCommand`, `InvalidNumericFieldValue`, flow YAML parse errors, `UnsupportedJavaVersion`, `CloudApiKeyNotAvailable` | `test_configuration_error` | High |
| Maestro killed by a signal or by CI cancellation (observed by the agent) | `process_terminated` | High (observed) |
| Agent internal failures; no device available; disk full; `DestinationIsNotWritable` | `infrastructure_failure` | Medium |
| Only a console regex matched | Category from the regex | **Low**. Mark it as inferred |
| Nothing matched | `unknown` | — |

Which exception class reaches `commands.json` or the logs, and in what form, is an **open Phase 0 question**. The exception names are verified in source; how they are serialized in artifacts is not documented. **[V]** and **[I]** respectively.

**Layer 2: fingerprint (grouping).** Use two levels:

```
signature   = sha256(grouping_version, exception_type, normalized_message_template)
              # groups the "same kind of failure" across flows, e.g. login button missing everywhere
flow_group  = sha256(grouping_version, flow_identity, step_identity, signature)
              # groups the "same failure of the same flow at the same step"
step_identity = command_type + (label if set) + normalized_selector
              # fallback: runFlow nesting path + command index — sequenceNumber alone shifts when flows are edited
```

**Normalization pipeline for Maestro messages** (Sentry-inspired; **[I]** for the Maestro-specific steps):

1. Strip ANSI codes and Logback prefixes. Collapse whitespace.
2. Replace volatile tokens:
   - absolute paths and artifact directories, which become `<path>`
   - timestamps and durations
   - device serials (`emulator-5554` becomes `<serial>`)
   - ports, IPs, URLs and UUIDs
   - hex and hash values
   - coordinates (`540,1200` and percentage points become `<point>`)
3. **Keep selector text by default.** In Maestro, `Element not found: Text matching regex: Login` differs meaningfully from `…: Checkout`. This is the key difference from Sentry's `quoted_str` rule. Parameterize only digits, dates and IDs *inside* selector values, which catches dynamic text such as "Order #48213".
4. For `application_crash`, fingerprint the crash, not the Maestro message:
   - Android: exception class plus the top 3 in-app frames from the `crash-report.txt` stack trace (package prefix = app ID), with line numbers dropped.
   - iOS: `.ips` JSON, using the exception type and the top in-app frames.
5. Store `grouping_version` with each fingerprint. Never regroup history silently. Allow manual **merge and unmerge** of groups.

**Layer 3: user rules.** Projects can add Sentry-style or Allure-style rules, first match wins. For example:

```
message:"*Unable to connect to backend*"            -> category=infrastructure_failure, group=backend-down
step.command:assertVisible selector.id:"promo_*"    -> group=promo-banner-timing, {{ default }}
```

Rules are versioned and audited (spec §10 audit). A rule-based category is displayed as **inferred**, not observed (spec §18).

**Encourage `label:` on key steps.** Maestro supports `label` for reporting **[V]** (docs "Labeling and error handling"). Labels give stable, human-readable step identities and make fingerprints survive flow edits.

---

## 9. Ideas worth borrowing

**Data model**

- **ReportPortal's item tree:** Launch, then Suite, Test and Step, with *nested steps that have no statistics* (`hasStats:false`). Add attributes (key and value), a `retry` flag on items, and `rerunOf` on launches. This maps one-to-one onto Fleet: Suite Execution, Flow Execution, Step, with `runFlow` as a nested step. The status set includes `interrupted` and `cancelled`. A default issue type of "To Investigate" for unclassified failures is an honest way to say "unknown". **[V]**
- **Allure 3 test identity:** `testCaseId` or `fullName`, plus parameters, plus **environment**. "Environments" link the same test across conditions. This is a good template for Fleet's flow identity plus condition key. **[V]**
- **Datadog `test.configuration.*` tags:** an extensible bag of condition dimensions. Note their warning that non-deterministic parameters create "new tests". **[V]**
- **Maestro Cloud webhook and JUnit properties:** a flow record with `status`, `failureReason`, times, `tags`, `properties`, device model, OS and locale, plus deep-link properties. Fleet's export JUnit or webhook could add `fleet.runUrl` the same way Maestro adds `cloud.runUrl`. **[V]**
- **OTel `cicd.*` and `test.*` names** for the CI context and status enums (§5.3). **[V]**

**UI patterns**

- **Spotify Odeneye grid** for Screen E: flows down the side, runs across the top, cells coloured by outcome. Add a per-device split so a column of failures on one emulator shows up as an infra incident at a glance. **[V]** for the pattern.
- **Expo's flow table:** pass rate, fails, flake rate, P90 duration and last run per flow, then a flow drill-down with error patterns. A compact Screen E default. **[V]**
- **Buildkite and Trunk monitor model:** a label is driven by an alarm and a recover rule with visible thresholds. Mutes have an expiry. Branch and tag filters are shown on the monitor. This is explainable, and it avoids permanent "flaky" tags. **[V]**
- **Allure "New failed", "New passed" and "Changed on retry" marks:** cheap, high-signal badges for Screen F comparisons and PR views. **[V]**
- **Device-farm device grid** (STF and GADS): state badge, freshness indicator, current assignment. Leave out streaming and remote control (spec non-goal). **[V]**
- **GitHub check run and job summary with a deep link:** keep the `mikepenz` or `dorny` style summary in the PR, and link each failing flow to Fleet's Flow Investigation page. This meets developers where they already look. **[I]**

**Operational lessons**

- **Don't depend on private interfaces of the test runner.** Sorry-Cypress was stranded when Cypress 11 and 13 blocked third-party recorders. `maestro-complete-reports` patches Maestro JARs and is pinned to Maestro 2.0.9 and 2.0.10, while the current CLI is 2.12.1. Fleet should rely only on documented outputs (JUnit, the `manifest.json` contract, `commands.json`, the console with a tolerant parser), wrap Maestro without modifying it, and keep version fixtures (spec §15). **[V]** for both facts.
- **Licensing traps:** GADS ships a proprietary, obfuscated UI under an AGPL core. ReportPortal moved SSO and LDAP to paid in 2026. Recheck open-source licence terms at each upgrade. **[V]**

---

## 10. Maestro-specific community tooling (survey)

| Project | What it is | Status and notes |
|---|---|---|
| `mobile-dev-inc/action-maestro-cloud` | Official GitHub Action for Maestro Cloud (v3.x). Outputs the console URL, upload status, per-flow results and binary ID; `async` mode | Active; 31 stars on the captured page [V]. Cloud-only, not useful for local emulators |
| `devicelab-dev/maestro-complete-reports` | Patched Maestro JARs adding JSON, HTML, JUnit and Allure reports with screenshots | 12 stars; pinned to Maestro 2.0.9 and 2.0.10 [V]. A cautionary example |
| `devicelab-dev/maestro-runner` (also EdgeApp and other forks) | A Go re-implementation that runs Maestro YAML, with HTML, JUnit and Allure-compatible reports and dynamic parallel distribution. Its driver agents are not open source | About 523 stars [V]. An *alternative runner*. Out of scope (Fleet must observe real Maestro). Its existence shows demand for reporting Maestro doesn't ship |
| DeviceLab blog: "Maestro test reports: JUnit, HTML, Allure" (2025-12-13) | Recipes: `mikepenz` action, Jenkins `junit`, GitLab `reports: junit`, Allure | No live progress [V] |
| Expo EAS Insights (Maestro tab) | Hosted per-flow pass rate, flake rate and P90 | Requires EAS Workflows and the Production plan [V] |
| devicecloud.dev, TestingBot | Commercial Maestro-compatible clouds; TestingBot streams live video and steps | [V] (vendor claims) |
| **Live Maestro progress for self-hosted CI** | **No open-source or commercial project found** | Searched GitHub-oriented and general queries; nothing found [I]. This supports the differentiated part of the spec |

I could not query the GitHub API directly from this environment (access was blocked). The GitHub survey relied on web search and fetched README pages, so smaller projects may have been missed. **[I]**

---

## 11. Summary of recommendations

1. **Go with Option A′ (focused build).** Build the agent, live state, Maestro-aware model, device and host correlation, and a lean evidence viewer. Keep history, flakiness and grouping deliberately simple and explainable. Don't build ML, a TSDB, TMS features or quality gates.
2. **Prototype Option B in Phase 0** with ReportPortal and Allure 3 on the real fixtures, to put a number on what "buy" gives. Keep RP and Allure exporters as optional adapters, not core.
3. **Adopt OTel naming now** and add an optional OTLP exporter later (Rust metrics SDK is stable). Let Grafana own fleet-wide capacity analytics.
4. **Flakiness v1** is two signals (confirmed same-commit inconsistency; suspected flip rate ≥ 0.2 over 20 default-branch runs) within a visible condition key. Infra-classified and environment-anomaly failures are excluded, plus a Spotify-style column rule for incidents. Labels never hide failures.
5. **Fingerprinting v1** uses the Maestro exception class, step identity and parameterized message (keeping selector text). Crashes use in-app frames. Grouping is versioned, user rules are first-match-wins, and manual merge and unmerge are available. Categories carry evidence and confidence.
6. **Use only documented Maestro surfaces.** These are JUnit (`junitId`, `properties`), `manifest.json`, `commands.json`, the logs, and the console via a tolerant parser. Never patch Maestro.
7. **Expose Maestro's data-egress settings** (`MAESTRO_CLI_NO_ANALYTICS`, `--analyze`, AI commands) in `maestro-fleet check`, so IT can enforce policy.

---

## Sources

**Maestro**
- Pricing: https://maestro.dev/pricing
- Cloud marketing page: https://maestro.dev/cloud
- Docs index: https://docs.maestro.dev/llms.txt
- Maestro solutions (Studio, CLI, Cloud): https://docs.maestro.dev/get-started/maestro-solutions.md
- Maestro Cloud overview: https://docs.maestro.dev/maestro-cloud
- Robin URL (now redirects to Maestro Cloud docs): https://docs.maestro.dev/cloud/robin
- Test reports and artifacts (manifest.json, commands.json, junitId, cloud properties): https://docs.maestro.dev/maestro-flows/workspace-management/test-reports-and-artifacts.md
- Automatic retries (smart retries): https://docs.maestro.dev/maestro-cloud/advanced-features/automatic-retries.md
- Webhooks (payload): https://docs.maestro.dev/maestro-cloud/notifications/configure-webhooks.md
- CLI commands and options: https://docs.maestro.dev/maestro-cli/maestro-cli-commands-and-options.md
- CLI environment variables (analytics, log pattern): https://docs.maestro.dev/maestro-cli/environment-variables.md
- AI test analysis (data routed via Maestro Cloud): https://docs.maestro.dev/maestro-flows/workspace-management/ai-test-analysis.md
- Releases: https://github.com/mobile-dev-inc/maestro/releases
- Changelog: https://raw.githubusercontent.com/mobile-dev-inc/maestro/main/CHANGELOG.md
- Exception taxonomy (source): https://raw.githubusercontent.com/mobile-dev-inc/maestro/main/maestro-client/src/main/java/maestro/Errors.kt
- Maestro Viewer release post: https://maestro.dev/blog/maestro-cli-v2-6-0
- GitHub Action for Maestro Cloud: https://github.com/mobile-dev-inc/action-maestro-cloud

**Test-results platforms**
- ReportPortal hardware requirements: https://reportportal.io/docs/installation-steps/HardwareRequirements/
- ReportPortal reporting developers guide: https://reportportal.io/docs/developers-guides/ReportingDevelopersGuide
- ReportPortal docker-compose: https://raw.githubusercontent.com/reportportal/reportportal/master/docker-compose.yml
- ReportPortal pricing: https://reportportal.io/pricing
- ReportPortal SaaS pricing: https://reportportal.io/pricing/saas
- ReportPortal "Refocusing" post (SSO and LDAP to paid, 2026-04-02): https://reportportal.io/blog/refocusing-reportportal-investing-in-core-automation-value/
- ReportPortal auto-analysis: https://reportportal.io/docs/analysis/AutoAnalysisOfLaunches/
- ReportPortal unique error analysis: https://reportportal.io/docs/analysis/UniqueErrorAnalysis/
- ReportPortal flaky widget: https://reportportal.io/docs/dashboards-and-widgets/FlakyTestCasesTableTop50/
- ReportPortal JUnit import: https://reportportal.io/docs/plugins/JUnit
- ReportPortal Quality Gates: https://reportportal.io/docs/features/QualityGates/
- Allure 3 overview: https://allurereport.org/docs/v3/
- Allure history and retries: https://allurereport.org/docs/history-and-retries/
- Allure test stability: https://allurereport.org/docs/test-stability/
- Allure Report Storage (self-hosted): https://allurereport.org/docs/self-hosted-storage/
- Allure categories: https://allurereport.org/docs/categories.md
- Allure TestOps pricing: https://qameta.io/pricing
- Allure TestOps defects: https://docs.qameta.io/allure-testops/briefly/defects/
- Buildkite pricing: https://buildkite.com/pricing
- Buildkite Test Engine monitors: https://buildkite.com/docs/test-engine/workflows/monitors
- Buildkite Test Engine workflows announcement: https://buildkite.com/resources/blog/introducing-test-engine-workflows
- Buildkite flaky test management: https://buildkite.com/docs/test-engine/flaky-test-management
- Buildkite JUnit import: https://buildkite.com/docs/test-engine/importing-junit-xml
- Datadog Test Optimization docs: https://docs.datadoghq.com/tests/
- Datadog pricing: https://www.datadoghq.com/pricing/?product=test-optimization
- Datadog validate optimizations (JUnit limitation): https://docs.datadoghq.com/tests/guides/validate_optimizations.md
- Trunk detection: https://docs.trunk.io/flaky-tests/detection
- Trunk pricing: https://trunk.io/pricing
- Currents pricing: https://currents.dev/pricing
- Cypress pricing: https://www.cypress.io/pricing
- Sorry-Cypress repo: https://github.com/sorry-cypress/sorry-cypress
- Cypress v13 blocks third-party plugins: https://alternativeto.net/news/2023/10/cypress-io-blocks-certain-plugins-from-version-13-pushing-users-to-alternative-frameworks
- BuildPulse pricing: https://buildpulse.io/pricing
- Testmo pricing: https://www.testmo.com/pricing/
- TestRail pricing: https://www.testrail.com/pricing/
- CloudBees Smart Tests docs: https://docs.cloudbees.com/docs/cloudbees-smart-tests/latest/features/test-trends-and-insights
- Launchable pricing (third party): https://www.softwareadvice.com/automation-testing/launchable-profile
- Tesults (third-party listing): https://www.everydev.ai/tools/tesults
- Flakiness.io (third-party catalog): https://testguild.com/tools/flakiness-io
- Develocity flaky test detection: https://docs.gradle.com/develocity/flaky-test-detection
- dorny/test-reporter: https://github.com/dorny/test-reporter
- mikepenz/action-junit-report: https://github.com/mikepenz/action-junit-report
- EnricoMi/publish-unit-test-result-action (third-party listing): https://dev.co/testing/open-source/publish-unit-test-result-action
- Expo EAS Insights for Maestro: https://docs.expo.dev/eas-insights/maestro/

**Device farms and clouds**
- DeviceFarmer STF: https://github.com/DeviceFarmer/stf
- GADS: https://github.com/shamanec/GADS
- BrowserStack Maestro video: https://www.browserstack.com/docs/app-automate/maestro/debug-failed-tests/video-recording
- BrowserStack Maestro logs: https://www.browserstack.com/docs/app-automate/maestro/debug-failed-tests/maestro-logs
- Sauce Labs Maestro (community, maestro-runner): https://docs.saucelabs.com/basics/community-frameworks/maestro/
- LambdaTest / TestMu AI HyperExecute Maestro docs (PR): https://github.com/LambdaTest/documentation/pull/996/files
- Genymotion SaaS automation: https://docs.genymotion.com/usage/saas/automation/
- Genymotion with Maestro (community guide): https://dev.to/retyui/use-genymotion-saas-to-run-maestro-e2e-on-ci-14hm
- Firebase Test Lab: https://firebase.google.com/docs/test-lab
- emulator.wtf docs: https://docs.emulator.wtf
- TestingBot Maestro: https://testingbot.com/features/automation/maestro
- devicecloud.dev: https://devicecloud.dev/

**Maestro community tooling**
- maestro-complete-reports: https://github.com/devicelab-dev/maestro-complete-reports
- maestro-runner: https://github.com/devicelab-dev/maestro-runner
- DeviceLab reports post: https://devicelab.dev/blog/maestro-test-reports-junit-html-allure

**OpenTelemetry and CI observability**
- Semantic conventions index (1.44.0): https://opentelemetry.io/docs/specs/semconv/
- CI/CD conventions: https://opentelemetry.io/docs/specs/semconv/cicd/
- CI/CD spans: https://opentelemetry.io/docs/specs/semconv/cicd/cicd-spans/
- CI/CD metrics: https://opentelemetry.io/docs/specs/semconv/cicd/cicd-metrics/
- CI/CD resource conventions: https://opentelemetry.io/docs/specs/semconv/resource/cicd/
- `cicd.*` attribute registry: https://opentelemetry.io/docs/specs/semconv/registry/attributes/cicd/
- `test.*` attribute registry: https://opentelemetry.io/docs/specs/semconv/registry/attributes/test/
- githubreceiver: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/receiver/githubreceiver
- hostmetrics receiver: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/receiver/hostmetricsreceiver
- otel-cli: https://github.com/equinix-labs/otel-cli
- opentelemetry-rust: https://github.com/open-telemetry/opentelemetry-rust
- SigNoz CI/CD observability guide: https://signoz.io/blog/cicd-observability-with-opentelemetry
- Grafana CI/CD posts: https://grafana.com/tags/ci-cd

**Flakiness literature**
- Google 2016, "Flaky Tests at Google and How We Mitigate Them": https://testing.googleblog.com/2016/05/flaky-tests-at-google-and-how-we.html
- Google 2017, "Where do our flaky tests come from?": https://testing.googleblog.com/2017/04/where-do-our-flaky-tests-come-from.html
- Meta 2020, "Probabilistic Flakiness": https://engineering.fb.com/2020/12/10/developer-tools/probabilistic-flakiness/
- Apple, ICSE-SEIP 2020, "Modeling and Ranking Flaky Tests at Apple": https://2020.icse-conferences.org/details/icse-2020-Software-Engineering-in-Practice/2/Modeling-and-Ranking-Flaky-Tests-at-Apple
- Flip rate definition: https://docs-flakiness-io.pages.dev/concepts/flip-rate
- Spotify 2019, "Test Flakiness": https://engineering.atspotify.com/2019/11/test-flakiness-methods-for-identifying-and-dealing-with-flaky-tests/
- FlakeFlagger, ICSE 2021: https://2021.icse-conferences.org/details/icse-2021-papers/115/FlakeFlagger-Predicting-Flakiness-Without-Rerunning-Tests
- Romano et al., UI-based flaky tests: https://arxiv.org/abs/2103.02669
- Thorve et al., Android flaky tests, ICSME 2018: https://people.cs.vt.edu/nm8247/publications/empirical-study-flaky-pdf.pdf
- Haben et al., Chromium CI flaky vs fault-triggering failures: https://arxiv.org/abs/2302.10594

**Failure grouping**
- Sentry event grouping: https://docs.sentry.io/concepts/data-management/event-grouping/
- Sentry fingerprint rules: https://docs.sentry.io/concepts/data-management/event-grouping/fingerprint-rules/
- Sentry parameterization (source): https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/grouping/parameterization.py
