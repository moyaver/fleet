# 09: Competitive review and feature roadmap check

**Research date:** 2026-11-06
**Scope:** checks two external drafts (a competitive landscape study and a feature roadmap, both written with ChatGPT on 2026-11-06) against primary sources, against Fleet's code at `1d678fc`, and against the spec, PLAN and ADRs. Decides which proposals become issues, why, and in which milestone.
**Builds on:** [07 Prior art and build-vs-buy](07-prior-art-and-build-vs-buy.md) and [08 Phase 0 results](08-phase0-results.md).

## How to read this report

Markers follow research 07:

- **[V]**: verified on 2026-11-06 against a primary source (vendor docs, pricing page, release notes, Fleet source or captured fixture). The URL or path is given.
- **[V3]**: third-party or search-excerpt only.
- **[I]**: inferred, or not verifiable today. Treat as a hypothesis.

Prices are public list prices on the research date. Absence from a vendor's public docs is not proof the vendor can't do it.

---

## 1. Verdict

1. **The drafts' main conclusion holds.** Fleet's defensible position is live, Maestro-native observability on CI runners and emulators the company already operates, tied to host and device context and retained evidence. Report viewing, flaky history, failure grouping, AI summaries and duration-aware splitting are commodity or crowded. **[V]** for the product facts below.
2. **No product found does live Maestro observability on your own runners.** BrowserStack's Maestro support runs only on BrowserStack's devices. Maestro Cloud runs its own devices. Datadog, Trunk, BuildPulse, Buildkite, Currents, Testkube, ReportPortal and Allure name no Maestro integration; the only path is JUnit import. No self-hosted, Maestro-specific dashboard was found on GitHub. **[V]** per product, **[I]** for "none exists".
3. **Several draft claims about competitors were overstated** (§3). Several claims about Fleet were right and point at real gaps (§4). The two most important: Fleet uploads less of Maestro's evidence than Maestro already writes, and Fleet has no installable release.
4. **Most of the drafts' 71 features are already in the spec or PLAN, already done, or excluded by an ADR** (§5). Sixteen new issues come out of this review (§6). Everything else is either covered by an open issue, already planned for M7+, or would need an ADR change first (§7).

## 2. What was checked, and how

| Question | Method |
|---|---|
| Are the drafts' claims about Fleet true? | Read the code at `1d678fc` and cite file:line (§4) |
| Are the competitor claims true? | Vendor docs, pricing and release notes fetched on 2026-11-06 (§3) |
| What evidence does Maestro actually write? | Captured fixtures `fixtures/maestro/2.12.1/`, Maestro docs and GitHub releases (§4.1) |
| Is each proposed feature new, planned, done or excluded? | Mapped against `docs/spec-v1.0.md`, `docs/PLAN.md`, ADR-0009, ADR-0011, ADR-0013 and open issues (§5) |

## 3. Competitor claims: corrections

Only claims that changed are listed. Unlisted claims checked out **[V]**.

| Product | Draft claim | Finding |
|---|---|---|
| BrowserStack App Automate | Live results and per-step logs for Maestro | Maestro runs only on BrowserStack's real devices; a local tunnel reaches private apps but not your hardware. Video is on by default; network logs are opt-in HAR downloads after the run. No live per-step streaming documented. **[V]** [docs](https://www.browserstack.com/docs/app-automate/maestro/overview) |
| BrowserStack Test Reporting & Analytics | AI failure analysis | Exists; analyses only evidence inside BrowserStack, with a fair-use cap of 30 analysed failures per 1,000 executions per month. No public price. **[V]** [docs](https://www.browserstack.com/docs/test-reporting-and-analytics/agents/test-failure-analysis) |
| Datadog Test Optimization | Correlates tests with logs and infrastructure; recommends the Agent on self-hosted CI | Pricing confirmed ($20/committer/month annual, $29 on-demand). The Test Optimization docs mention traces and RUM, not host infrastructure; the "install the Agent" recommendation was not found. No Maestro support (JUnit XML is the path). **[V]** [pricing](https://www.datadoghq.com/pricing/?product=test-optimization), [setup](https://docs.datadoghq.com/tests/setup/) |
| Buildkite Test Engine | Accepts results from other CI; splitting | Splitting by historical timing is real but documented through `bktec` on Buildkite. Outside Buildkite, only the REST API path is documented. Free tier 250k executions/month; Pro $30/active user. **[V]** [splitting](https://buildkite.com/docs/test-engine/test-splitting), [pricing](https://buildkite.com/pricing/) |
| ReportPortal | SAML/LDAP move to paid from 26.1 | Post dated 2026-04-02 says SAML and LDAP move to paid "with the release of TMS and Organizations"; the 26.1 cutoff is for Active Directory. No flaky-test page found in the docs. No Maestro integration. **[V]** [blog](https://reportportal.io/blog/refocusing-reportportal-investing-in-core-automation-value/) |
| Allure Report 3 | Serious lightweight alternative | Current version 3.33.0. History, flaky detection, quality gates and the free self-hosted Storage service are real. No official Maestro integration among 57 modules. **[V]** [modules](https://allurereport.org/modules/) |
| Allure and Maestro JUnit | Allure 3.22.1 showed Maestro failures as passes | Research 08 measured this once on 3.22.1 (5 of 5 shown passed, shards folded into retries). Maestro 2.12.1 writes failing cases as `status="ERROR"` plus a `<failure>` child (`fixtures/maestro/2.12.1/fail/run/report.xml:4-5`). Not re-run on 3.33.0. **[V]** for the fixture, **[I]** for current Allure behaviour |
| Maestro | Upstream event stream would help competitors | Latest CLI is 2.12.1 (2026-11-02). Still no documented event or reporter API. Open PR #3221 adds JAR plugins with lifecycle hooks; not merged, not a stream. `--analyze` (beta) writes an AI HTML report and needs a Cloud account. **[V]** [releases](https://github.com/mobile-dev-inc/maestro/releases), [PR](https://github.com/mobile-dev-inc/Maestro/pull/3221) |
| Expo EAS Insights | Maestro history and flake rate | Real, but Production and Enterprise plans only, and only for EAS Workflows `maestro` jobs. "Flaky" means passed after a retry. **[V]** [docs](https://docs.expo.dev/eas-insights/maestro/) |
| Testkube | Universal platform | Pro $700/month per runner agent confirmed. No Maestro support found. **[V]** [pricing](https://testkube.io/pricing) |
| Playwright | Perfetto reporter | Correct: built-in `perfetto` reporter since 1.63. **[V]** [release notes](https://playwright.dev/docs/release-notes) |

Native CI reporting is as the draft says: GitLab compares new and resolved failures per merge request with 14 days of failure history; Azure Pipelines has test analytics; GitHub Actions has job summaries but no native test-results UI. **[V]** [GitLab](https://docs.gitlab.com/ci/testing/unit_test_reports/), [GitHub](https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions)

## 4. Claims about Fleet

| Draft claim | Verdict | Evidence |
|---|---|---|
| Flaky history doesn't apply infrastructure exclusions | **True.** Runs in state `interrupted` are filtered, but infrastructure failures recorded as `failed` count toward flakiness; the loader hardcodes `exclusion: None` and nothing produces an exclusion. | `crates/fleet-store/src/screens/history.rs:373`, `crates/fleet-core/src/flaky.rs:28-33`, `migrations/0007_analytics.sql:37-38` |
| Agent uploads only screenshots, commands, JUnit, maestro.log | **True.** Five artifact classes exist; the agent produces four. Hierarchy is registered as an event only. | `crates/fleet-protocol/src/artifact.rs:21-54`, `crates/fleet-agent/src/session.rs:562-586` |
| Device samples not ingested; host pressure metrics missing | **True.** Nothing writes `device_samples` outside a test; the sample frame has no load, PSI, swap or core count. Tracked as #39 and #42. | `crates/fleet-protocol/src/frame.rs:144`, `crates/fleet-store/src/screens/samples.rs:158` |
| Nested steps mis-paired | **True,** #62 (PR in flight). | `crates/fleet-agent/src/session.rs:433-478` |
| Flow identity splits on absolute paths (#64) | **Outdated.** Fixed in #65 (2026-11-05). | `crates/fleet-store/src/ingest/apply.rs:585-604` |
| Concurrent runs on one host mis-attribute metrics | **Partly.** The sampler measures the one PID the supervisor spawned, so per-run attribution holds; emulator (qemu) load isn't measured at all (PLAN §6.5). | `crates/fleet-agent/src/observer.rs:282`, `crates/fleet-agent/src/metrics.rs:39-100` |
| Retry semantics not distinct | **Partly.** CI re-attempts and shards/devices are distinct. Maestro's `retry` command is parsed and discarded. Tracked in #38. | `crates/fleet-maestro/src/log.rs:154,209-212` |
| No released installer | **True, and worse.** No GitHub release or tag exists, yet `examples/github-actions/fleet-agent.yml` downloads `releases/latest`, so the documented CI setup fails today. Releases carry build attestations, not the minisign signature PLAN §11.2 describes. | `README.md:18`, `.github/workflows/release.yml`, `examples/github-actions/fleet-agent.yml` |
| Maestro version compatibility not enforced | **True.** Only `fleet-agent check` probes the version; `run` never does, and the fingerprint's `maestro` condition is always `None`. Fixtures cover 2.12.1 only. | `crates/fleet-agent/src/commands.rs:70-100`, `crates/fleet-store/src/ingest/analytics.rs:100` |
| Overhead measured once | **True.** 0.14% comes from one 50 s run on an M1 Pro; PLAN makes a repeatable overhead benchmark an exit criterion. | `docs/src/runner-guide.md:23-25`, `docs/PLAN.md:584` |
| No PR summary, notifications, MCP, OTLP, catalog, saved views, source links | **True.** None found. | grep over `crates/`, `examples/`, `.github/` |

### 4.1 Evidence Maestro writes that Fleet drops

From the 2.12.1 captures (`--format junit --flatten-debug-output --debug-output`):

| Artifact | When Maestro writes it | Fleet today |
|---|---|---|
| `screen-hierarchy/step-NNN-*.json` | Failing step | Event only, not uploaded |
| `logs/device-logcat.txt` | Every flow (0.5 MB passing, 3.9–7.3 MB failing) | Ignored |
| `commands.json` `artifacts[]` | Every step that produced a file | Parsed, but the step-to-screenshot link is unused |
| Per-flow `logs/maestro.log` | Every flow | Ignored (the session log is uploaded) |
| Crash and ANR reports, `takeScreenshot`, `startRecording` video | Only when they happen or the flow asks | Ignored, and **no fixture captures them** |

Each needs a redaction rule first (I6): hierarchy and logcat hold on-screen text and app logs; video can't be redacted at all.

## 5. Where the 71 proposed features land

Summary of the mapping against the spec, PLAN, ADRs and open issues.

- **Already done** (13): run comparison, fingerprints, flaky v1, history, evidence retention and GC, OIDC, roles, S3/filesystem storage, spool and replay, provenance, live health sweeps, OpenAPI, protocol schema.
- **Already specified, open issue exists**: nested steps and retries (#62, #38), device telemetry (#39), host pressure and indexes (#42), Maestro HTML upload (#43), multiplexed live socket (#40), nightly real-Maestro regression (#45), Fleet Planner (#15).
- **Already specified in PLAN, no issue yet**: GitHub composite action (PLAN §11), backup runbook (PLAN §12, M6), repeatable overhead benchmark (PLAN §12 exit criterion). These become issues now (§6).
- **Excluded by an ADR, not opened**: Fleet-generated HTML report and quality gates (ADR-0009: "Don't build … a report generator … quality gates"); Playwright, Appium, Espresso, XCUITest and generic JUnit/CTRF ingest (ADR-0011: v1 is Maestro only); dynamic scheduling (ADR-0013: static only); automatic reruns and autonomous AI debugging (spec non-goals). Any of these needs a new ADR before an issue.
- **Planned for M7+ in PLAN, left there**: notifications, OTLP export, iOS, AI summaries with approval.
- **New and in scope**: evidence completeness, flaky exclusions, Maestro version recording, a working release, reproduction and rerun manifests, agentless Maestro import, triage links, a flow catalog. These become issues (§6).

One inconsistency surfaced: the spec lists quality-gate recommendations under Phase 6 (`docs/spec-v1.0.md` ~l.1114) while ADR-0009 excludes quality gates. ADR-0009 is the later decision; the spec line should be reconciled when the spec is next revised.

## 6. Decisions: what, why, how, when

**When** follows one rule: anything that affects whether a displayed result or piece of evidence is correct, or whether someone can install Fleet at all, lands in **M6** (v1.0). Workflow features that help people use correct data land in **M7+**, after v1.0 and after the comparative study says where Fleet actually wins.

### M6: trust and adoption (before v1.0)

| Item | What | Why | How (sketch) |
|---|---|---|---|
| 1, [#70](https://github.com/moyaver/fleet/issues/70) | First tagged pre-release, and a working install path | The documented CI example fails today (§4). Nothing else matters if it can't be installed. | Tag a `v0.x` pre-release through the existing cargo-dist workflow; make the example pin it; decide attestation vs minisign per PLAN §11.2 |
| 2, [#71](https://github.com/moyaver/fleet/issues/71) | GitHub composite action with a job summary | PLAN §11 specifies the action; a summary puts failing flows and Fleet links where developers already look. Writes to `$GITHUB_STEP_SUMMARY`, never to the CI log (I1). | `action.yml` wrapping install plus `fleet-agent run`; the agent appends a short Markdown summary at run end |
| 3, [#72](https://github.com/moyaver/fleet/issues/72) | Infrastructure exclusions in flaky analysis | A failure caused by a dead emulator currently counts as flakiness, which is a false signal (I3 spirit). ADR-0009 promises exclusions. | Produce `Exclusion` from classified infrastructure outcomes and conditions; test with the `device-killed` fixture |
| 4, [#73](https://github.com/moyaver/fleet/issues/73) | Record the Maestro version per run | Version-profile degradation (PLAN) and the fingerprint's `maestro` condition both need it; today only `check` knows. | Probe once at `run` start (cached), send it in the run hello, warn on unknown profiles |
| 5, [#74](https://github.com/moyaver/fleet/issues/74) | Upload the failing step's UI hierarchy | Maestro already writes it; it explains most "element not found" failures. | New artifact class, redaction of text values per I6, linked to the failing step |
| 6, [#75](https://github.com/moyaver/fleet/issues/75) | Bounded, redacted logcat for failed flows | Crashes and app errors show up here first; full files are 4–7 MB and unredacted. | Upload a window around the failing step, size-capped, through the redactor |
| 7, [#76](https://github.com/moyaver/fleet/issues/76) | Link step screenshots through `commands.json` `artifacts[]` | Removes filename guessing; small. | Parse the field already in the fixtures |
| 8, [#77](https://github.com/moyaver/fleet/issues/77) | Capture the missing Maestro fixtures | Crash, ANR, `takeScreenshot`, `startRecording`, `repeat` and nested `runFlow` have no captures, and AGENTS.md forbids inventing output. Items 5–7, #38 and #62 need them. | Run the capture scripts on an emulator; add scenarios |
| 9, [#78](https://github.com/moyaver/fleet/issues/78) | Repeatable agent overhead benchmark | 0.14% is one run; I10 needs a regression gate. | An xtask command that runs a fixture suite with and without the agent and reports CPU and RSS |
| 10, [#79](https://github.com/moyaver/fleet/issues/79) | Backup and restore runbook with a tested restore | PLAN M6 lists it; operators need it before trusting a v1.0. | Document `pg_dump` plus blob store copy; a CI or script check that restores into a fresh stack |
| 11, [#80](https://github.com/moyaver/fleet/issues/80) | Comparative diagnosis study | Every superiority claim is unproven (drafts §8). Decides which M7+ items are worth building. | Same Maestro fixtures through Allure 3.33 (re-test the misclassification), ReportPortal JUnit import, CI-only, and Fleet; measure correctness and time to the right next action; publish losses too |
| 12, [#81](https://github.com/moyaver/fleet/issues/81) | Planner: planned vs actual, against an equal-count baseline | ADR-0013 promises it; it is the only honest measure of whether the planner helps. After #15. | Compare plan totals to observed shard durations and show the imbalance |

### M7+: workflow on top of trusted data

| Item | What | Why | How (sketch) |
|---|---|---|---|
| 13, [#82](https://github.com/moyaver/fleet/issues/82) | Reproduce and rerun manifests | Turns a failed run into the next action without rebuilding CI context; reuses the ADR-0013 plan format, and Fleet still doesn't execute anything. | "Copy maestro command" on a flow; "export failed flows" as a plan manifest the agent's `--plan` accepts |
| 14, [#83](https://github.com/moyaver/fleet/issues/83) | Agentless import of a Maestro output directory | Lowest-friction way to try Fleet; ADR-0011-compatible because it is Maestro output only. | `fleet-agent publish <dir>` reusing `inspect`'s parser, with provenance marked as reported after the fact |
| 15, [#84](https://github.com/moyaver/fleet/issues/84) | Link a failure fingerprint to an external issue, with known/resolved state | Stops the same failure being investigated twice; ADR-0009 keeps triage thin, and this is the thin version. | A URL and state on the fingerprint, shown on screens D and E; no comments, no workflow |
| 16, [#85](https://github.com/moyaver/fleet/issues/85) | Flow catalog with richer filters | Today a flow exists only after it has run; filters by device model, OS, runner and fingerprint are missing. Saved views are just URLs. | Discover flows from the workspace or plan, attach history, keep filters in the query string |

## 7. Not opened, and why

| Proposal | Reason |
|---|---|
| Quality gates, quarantine | ADR-0009; quarantine also conflicts with research 07's "a flaky label never hides a failure" |
| AI assistant, LLM root cause | Spec non-goal for autonomous debugging; PLAN M7+ allows AI summaries only with approval. Revisit after item 11 |
| Read-only MCP server | Not excluded, but it should expose trustworthy analysis, so items 3–8 and 11 come first. Revisit in M7+ |
| Playwright and other frameworks, generic JUnit/CTRF | ADR-0011 |
| Dynamic scheduling, test selection by code change | ADR-0013 and the spec's "Fleet doesn't run tests" boundary |
| Infrastructure event detection, related-failure clustering, timeline overlays | Largely specified (spec screens C and D) and partly built. New analysis waits for item 11 to show it changes diagnosis |
| Notifications, OTLP export, iOS | Already planned for M7+ in PLAN; issues opened when M7+ starts |

## Sources

**Competitors**
- BrowserStack Maestro: https://www.browserstack.com/docs/app-automate/maestro/overview
- BrowserStack failure analysis: https://www.browserstack.com/docs/test-reporting-and-analytics/agents/test-failure-analysis
- Datadog pricing: https://www.datadoghq.com/pricing/?product=test-optimization
- Datadog Test Optimization setup: https://docs.datadoghq.com/tests/setup/
- Buildkite splitting: https://buildkite.com/docs/test-engine/test-splitting
- Buildkite pricing: https://buildkite.com/pricing/
- Currents pricing: https://currents.dev/pricing
- BuildPulse pricing: https://www.buildpulse.io/pricing
- Trunk flaky tests: https://trunk.io/flaky-tests
- ReportPortal hardware: https://www.reportportal.io/docs/installation-steps/HardwareRequirements/
- ReportPortal SaaS pricing: https://reportportal.io/pricing/saas
- ReportPortal licensing change: https://reportportal.io/blog/refocusing-reportportal-investing-in-core-automation-value/
- Allure modules: https://allurereport.org/modules/
- Allure self-hosted storage: https://allurereport.org/docs/self-hosted-storage.md
- Allure releases: https://github.com/allure-framework/allure3/releases
- Expo EAS Insights for Maestro: https://docs.expo.dev/eas-insights/maestro/
- Testkube pricing: https://testkube.io/pricing
- Maestro pricing: https://www.maestro.dev/pricing
- Playwright release notes: https://playwright.dev/docs/release-notes
- GitLab unit test reports: https://docs.gitlab.com/ci/testing/unit_test_reports/
- GitHub workflow commands: https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions

**Maestro**
- Releases: https://github.com/mobile-dev-inc/maestro/releases
- Plugin hooks PR (unmerged): https://github.com/mobile-dev-inc/Maestro/pull/3221
- CLI options: https://docs.maestro.dev/maestro-cli/maestro-cli-commands-and-options.md
- AI analysis (beta): https://docs.maestro.dev/maestro-flows/workspace-management/ai-test-analysis.md
