# Research 16: Fleet as a hub for mobile e2e tests (the plan)

**Date:** 2026-11-09. **Approved by the owner 2026-11-09.** Code read at v0.1.0-alpha.4 (PR #186).

**The one change from the planning session.** The owner decided: **no Fleet-owned schedules.** Option B (an opt-in Fleet schedule that dispatches a workflow) and its cron row are removed. The team's CI cron is the "when"; `--selection` is the "what". Every section that mentioned the schedule was edited to match. The rest of the text is the approved plan, verbatim. The effort figures are the approved ones and were not re-estimated for the removal (it was about 2 days).

Issues cite "the plan §N" for the section numbers below. Those references resolve to this file.

---


## 1. Product

**One line.** Fleet is the home for your mobile end-to-end tests. Run them in your own CI, watch them, organize them, and choose what runs and when. Every result lands on one record per app build, with infra failures set apart.

**GitHub description (167 chars):** "The home for your mobile end-to-end tests: run them in your CI, watch them, organize them, choose what runs and when. One record per app build. Maestro first. AGPL-3.0"

**Who it is for.** Mobile teams that run Maestro in their own CI and sign off releases by hand from CI pages and Slack. Android today, iOS next, other frameworks shallow after that. README line two: *Self-hosted alpha today; no hosted tier yet. Android today; iOS next.*

**The founder's sentence, word by word**

| Word | Day one (alpha.4 + W1) | Later waves | Status |
|---|---|---|---|
| **run them on CI** | `fleet-agent run -- maestro test …` in the team's own CI; result and files untouched (I1) | token-only hosted tier (W2) | exists |
| **watch them** | live view (overview, pipeline, flow, runner, device), evidence per failure | the "last night" page with incidents, infra label and blind banner (W4) | exists; the live view stays in the README because the founder asked for it |
| **organize them** | catalog, tags, history, triage, compare | **release page keyed by the build key** (W1), one record for Android and iOS (W3), quarantine with owner and expiry (W5), other frameworks on the same record (W7) | catalog exists; the rest is new |
| **pick what to run** | the planner's shards and the failed-flows manifest | guarded rerun of chosen flows (W6), then `--selection` = PR gate minus quarantine (W5) | new |
| **and when** | the team's CI cron calling `--selection` | none: the team's CI cron is the schedule (owner decision, 2026-11-09) | new (W5) |

**Day-one deliverables after alpha.4 (first three waves):** the build key and release page; the hosted token-only tier with 30-day retention and delete-my-data; iOS Maestro `publish` from a Mac. Then the "last night" page, quarantine, the guarded rerun and selection, shallow ingest, measurement, billing.

**Not planned:** Fleet running tests or providing devices, AI root cause, test selection by code change, per-seat/committer/device-hour pricing, pixel redaction, guessing at leftover test state, automatic retries (spec §17 stays for everything but dispatch), Fleet-owned schedules, SOC 2 and EU residency in this plan's horizon (the README says so).

---

## 2. Architecture shift to hosted

**Principle:** hosted is a config section on the same AGPL binary. Everything stays self-hostable.

| Concern | Exists (alpha.4) | New |
|---|---|---|
| **Tenancy** | `projects`, `users` (OIDC), `role_bindings`, `group_mappings`, `api_keys` per project, `agent_tokens`, `publisher_trust_policies`; `project_policy` (retention per artifact class, artifact bytes/files per day, sample intervals) | Migration 0034: `orgs`, `org_members`, `projects.org_id`. One Postgres, one schema (schema-per-org would multiply migrations and partitions and lose the `.sqlx` checks). Slugs stay globally unique, so no route changes. A self-hosted install gets one org, backfilled. **Rule change (ADR-0017):** ADR-0014 allowed editing migrations in place because no database outside development held them; the W2 beta ends that premise. From W2 migrations are append-only, 0034 is the last one edited in place, and the upgrade rehearsal starts from the beta's tag. Hosted mode refuses viewer tokens (they read every project). Scoping: every project route already goes through `read_project`/`require` then `fleet_core::policy::authorize`; id-keyed routes look up the project first; no role means 404 "no such project". The hole is `Grant { project: None }` (every project): at sign-in `org_members` expands into explicit per-project grants, `None` stays for the self-host operator only and hosted mode refuses it in config; creating a project refreshes the session's grants. Store queries keep taking a project id; none takes an org. **I14** proven by an e2e that calls every route in `openapi.json` as org A with org B's ids and gets 404. |
| **Signup and tokens** | OIDC browser login; `fleet api-key create`; GitHub OIDC exchange for agents | Hosted sign-in through a **GitHub App** (user OAuth; the rerun needs the App anyway). Signup creates an org, a project and an API key, so the agent needs only `FLEET_TOKEN`. Self-hosted keeps OIDC. Limitation on record: hosted sign-in requires a GitHub account; teams without GitHub Actions get no dispatch. |
| **Billing and metering** | nothing | `usage_monthly(org, month, flow_results)` upserted when a flow ends (lands with tenancy). Over quota: a banner; ingest is never refused. **Stripe** Checkout and the customer portal as redirects (no JS, I9); a signed webhook sets `orgs.plan`. Built last, after three design partners agree to a price (#205). Flat tiers by monthly flow results is a hypothesis, not a price. |
| **Storage, retention, delete** | partitions dropped by `store::retention::drop_expired`; `gc::expire_artifacts`; blobs keyed `(project_id, sha256)` so tenants never share a blob; runner-side redaction (I6); per-class retention and opt-outs | Hosted keeps 30 days instance-wide via the existing partition drops. `store::org::purge` removes an org; test: after a purge no table with a `project_id` column holds a row for that org, and no blob remains. |
| **Hosting** | Compose: Postgres 18, Garage, Caddy; distroless image | One VM running Compose, an S3 bucket, a separate usercontent domain (I7), nightly off-site `pg_dump` with a restore test. Owner provides domains, Stripe, terms, privacy policy, DPA, on-call. Cost ≈ $30–55/month (4 vCPU / 8 GB VM $20–40, bucket $5–10, two domains $3, uptime ping free), owner to confirm on vendor pages. Backups: nightly `pg_dump` plus bucket versioning, both to a second provider; `deploy/compose/backup-restore-check.sh` already exists and a weekly cron restores last night's dump into a throwaway Compose project, checks `/readyz`, the artifact count and one known run, then pings a dead-man switch. Runbook in `docs/src/hosted-ops.md` (public). |
| **Self-hostable** | everything | everything; self-hosted keeps OIDC, viewer tokens, one org. #177 (Postgres 16/17) and #178 (minimal profile) stay as pilot enablers, Later. |

---

## 3. Features: design and reuse

### 3.1 Build key and release record (W1)
- **Key** = `(app_id, platform, version, build)` + commit SHA (already in `ci.rs`). Sources, ranked: (1) **the device**, what actually ran: Android `dumpsys package <appId>` through `adb.rs::shell`; iOS `simctl appinfo` then `plutil` on `Info.plist`; the appId comes from `Launch app "…"` or `commands.json`; (2) `--app <apk|ipa|.app>`: a hand-written AXML parser over the workspace `zip` dependency (aapt is not on runners); IPA through `plutil`, Mac only; (3) `FLEET_BUILD`, user-declared. Travels as a new additive event `build.detected {app_id, platform, version, build, source}`. Fixtures: a `dumpsys-package.raw` capture and the APK that the anr-app's `build.sh` produces.
- **Server:** `builds(project, key, app_id, platform, version, build, source, sha)`; runs attach through `build.detected` and through JUnit ingest. **Release page:** one record per build showing every run, its runner and source. #172 (wasm size) ships here: a release captain opens the link cold.

### 3.2 iOS publish from a Mac (W3, #206)
Capture study first: a throwaway simulator in its own `--set` running `com.apple.Preferences` (nothing is built; this Mac has Xcode 27.1, iOS 18/26 runtimes, Maestro 2.12.1, xcpretty, no Gradle). Scenarios `ios-pass|fail|suite|sigkill|no-device` under `fixtures/maestro/2.12.1/`. Expected work: gate logcat, crash and probes to Android, handle UDID serials, keep the log parsers if the capture confirms them. The adb rule shows "not applicable on iOS", never "no infra". Live iOS runs follow nearly free (the agent already builds for darwin). Build-key timing: under `run` on a Mac, read after the first `Launch app` completes while the simulator is alive (`simctl appinfo`, `plutil`). Under `publish` the laptop may have rebuilt the app, so the ranking is `--app` > device read tagged `source: device_after_run` > `FLEET_BUILD`; with none, the run shows "build unknown" and the build is never guessed. Android `publish` follows the same rule; the guide recommends `run` over `publish` on laptops. On iOS the adb probes (`Probes::start_due`, `request_activity`), `adb::track` and the qemu scan are gated off; platform comes from argv (`-p ios`, `--udid`) confirmed by the session log. iOS has no device signals: iOS infra starts as the cascade rule only, and the banner says "device signals: not applicable on iOS".

### 3.3 "Last night" page (W4): the withdrawn pivot, reused
The rules move into `fleet-core::diagnosis` as pure functions over journal events. The agent runs them for the job summary and `fleet-junit.xml`; the server runs them at ingest, writes results to columns, and recomputes history when a rule changes (derived provenance, I4). Contents: failures grouped by **device incident** (#199), the **suspected-infra label** from three signals written before tuning (#198: adb leaves `device` during the flow; same-appId shards pass `launchApp` while this one fails it; the foreground read shows launcher, ANR or nothing; `app.crashed` never counts), one device-lost rule (#197), offline probes journalled as `device.probed` (#195), the **blind banner** (#192: exact `TESTED` list + anchor rule; `fleet-blind` always present), `maestro-watch` (#191), fixtures `label.json` (#193), capture study (#196), `fleet-junit.xml` (#201). **Definition:** each project has a morning cutoff (default 07:00 in the project's time zone). For each workflow on the default branch, the page shows its latest attempt that started in the 24 hours before the cutoff; one section per workflow, failures first, earlier attempts linked. A workflow that ran the night before but not in this window shows "did not start". Incidents are grouped across workflows. Needs `workflow` and `event` as new optional fields in `CiContext` (the agent adds them) and two new `pipeline_runs` columns.

### 3.4 Quarantine (W5, ADR-0018)
`quarantines(project, flow, owner, reason, expires_at)`. A quarantined flow **still runs and still shows red**; it is grouped separately on the last-night page and left out of flakiness and of the PR selection only (**I13**). Infra failures never create quarantine. Plans Fleet writes name the quarantined flows. This reverses ADR-0009's "no test management" for this one feature, with the buyers' reasons on record.

### 3.5 Guarded rerun and selection (W5–W6, ADR-0019)
- **Rerun:** `reruns(attempt, sha, ci_run, flows[], build_key, requested_by)`; flows checked against the catalog. The `workflow_dispatch` input is **one rerun UUID** (dispatch refs cannot be SHAs). The workflow runs `fleet-agent rerun fetch <id>` (authenticated with the job's CI OIDC token, so the customer's repo holds no Fleet secret), checks out that SHA, downloads the built APK, runs `run --rerun <id>`. The guard sits in `prepare.rs` and closes today's gap (`read_plan` appends paths unchecked): a path must be a normal relative path, not start with `-`, and be in the catalog. If the installed build key differs from the manifest's, **rerun mode** exits 2 before Maestro starts; `run` proper never exits early (I1). GitHub cannot limit the App's `actions: write` to one workflow, so Fleet enforces and audits the one-workflow limit itself. The job summary's rerun line (#200) is the tokenless fallback.
- **Selection:** `--selection pr-gate` fetches the PR selection (catalog minus quarantine) the same way. Failure path: if the selection cannot be fetched or fails validation, the agent runs the command exactly as given, prints one notice and journals `selection.unavailable`, so the last-night page shows the run as unselected (I13). `--rerun` exits 2 instead.

### 3.6 Shallow ingest (W7, ADR-0020)
`fleet-agent publish --junit <files> --framework espresso|xcuitest` with a generic quick-xml reader; `runner.detected` already names the framework; results attach to the build key. XCUITest fixture captured through xcpretty; Espresso needs Gradle, an owner install (the agent never lets `gradlew` download it). BrowserStack and FTL APIs after, dated when a partner needs them.

### 3.7 Measurement (W8)
`xtask labelled-runs` + nightly labelled leg + generated `accuracy.md` (#202) for construction accuracy; **in-hub verdicts** (#203 rewritten): triage marks a failure infra or real, and Fleet reports the label's recall and false positives per project. No export of derived tables.

---

## 4. Waves, acceptance, effort (one maintainer)

| Wave | Content | Acceptance | Days | Owner needed |
|---|---|---|---|---|
| **W0** | Merge #186; tag `v0.1.0-alpha.4` (no freeze steps); docs pass 1 (ADR-0016, README, PLAN §3, ROADMAP, milestones); #205 validation starts | CI green; tag published; README states the hub | docs 2 | tag; start the 10 conversations |
| **W1** Build key + release page | agent `build.detected`; `builds`; release page; #194; #172 | agent: `build.rs` on `fixtures/adb/android-37.1/dumpsys-package.raw` and a committed anr-app `app.apk` (AXML versionName/versionCode); macOS plist test; `tests/it/build_key.rs` (fake adb → `source: device`; no adb → no event, run unchanged, I2); `xtask schema`; `layouts.rs` (#194). server: `builds::a_build_detected_event_attaches_its_run`, `two_runners_one_key_one_build`, API `release_page_groups_runs_by_source`, browser `dashboard_release`; a brotli size budget in `xtask ci` (#172) | agent 4+1, server 5+2 | — |
| **W2** Hosted tier (private beta) | 0034 tenancy, GitHub App sign-in, token-only signup, metering, purge, hosted instance, `hosted-ops.md` | `cross_org.rs` (seed orgs A and B; as A call every `openapi.json` operation with B's slug and ids and subscribe to B's live socket: all 404, no byte of B's data; a route missing from the id map fails the test); `purge_leaves_no_project_rows` (walks information_schema); `hosted_refuses_viewer_tokens`; `signup_creates_org_project_key` against a wiremock GitHub; `usage_counts_each_flow_once` under the loadgen duplicates scenario; the weekly restore drill | server 8+1+3+3, docs 1 | domains, VM, S3, GitHub App, terms/privacy/DPA (#209) |
| **W3** iOS publish | #206 capture study, `publish` from a Mac, UDID serials | `inspect.rs` `ios-*` snapshots; publishing `ios-fail` to the fake server; no probe fires for a UDID | agent 6 | Mac time |
| **W4** Last night | #191–#199, #201 in `fleet-core::diagnosis`; `CiContext.workflow/event`; last-night page | agent: `versions.rs`, `diagnosis.rs`, `blind.rs`, observer faults in `supervisor.rs`, `summary.rs`, `junit_out.rs`, the kill captures. server: `last_night_groups_by_incident`, `blind_run_shows_banner`, `rule_change_recomputes_history` | agent 19, server 5 | Mac time for captures (#196); google_apis 33 install |
| **W5** Quarantine + selection | ADR-0018; `quarantines`; `--selection` | agent: `prepare.rs` path guards against `../x`, `-e`, absolute paths and paths outside the catalog for `--plan`, `--rerun`, `--selection`; wiremock tests for a selection fetched, timed out, refused. server: `quarantine_never_changes_outcome` (proptest: outcome columns identical with and without quarantine), `expired_quarantine_reads_as_none`, `selection_names_excluded_flows` (I13), audit rows | server 4, agent 2 | — |
| **W6** Guarded rerun | ADR-0019; `reruns`; `rerun fetch`; guard in `prepare.rs` | agent `tests/it/rerun.rs`: a build mismatch exits 2 and `fake-maestro.sh` never starts; a match runs; `rerun fetch` works with OIDC. server: `rerun_rejects_flows_outside_catalog`, `rerun_fetch_needs_the_policy_repo` (existing fake OIDC signer), `dispatch_calls_only_the_named_workflow` (wiremock) | agent 4, server 6+2 | GitHub App `actions: write` |
| **W7** Shallow ingest | ADR-0020; `publish --junit`; XCUITest fixture | agent `tests/it/junit_ingest.rs` on a captured xcpretty report (Gradle one once the owner installs Gradle); server `junit_ingest_attaches_to_build` | agent 4 | Gradle install for an Espresso fixture |
| **W8** Measured + gate | #202, #203 verdicts, hub release gate | accuracy.md generated; recall per project shown with its counts | agent 5, docs 1 | — |
| **Billing** | Stripe redirects + webhook | checkout → plan set; portal; banner over quota | server 4 | Stripe account; price agreed by 3 partners |

Totals ≈ agent 42, server 42, docs 6: **about 90 engineer-days** for one maintainer. Critical path: W1 → W2 → W3 (the CTO persona's four items) → W4 (the platform-lead persona's items).

---

## 5. Remaining work and issues

**PR #186 / alpha.4:** merge and tag now, as an ordinary release; drop the pivot's freeze steps (no `server-archive-1`, no manual-only image.yml). Full CI stays.

**ADRs**

| ADR | Decision | Supersedes / amends |
|---|---|---|
| 0016 | Fleet is a hub; the build key and release record are the core entity | Supersedes 0009 (scope); amends spec §1, §17 |
| 0017 | Hosted multi-tenant tier on the same AGPL binary: `org_id`, GitHub App sign-in, Stripe redirects, 30-day retention, purge | Amends 0008; answers research 12 §1.3 |
| 0018 | Quarantine with owner and expiry, infra excluded, I13 | Reverses ROADMAP "not planned: quarantine" and ADR-0009's "no test management" for this feature |
| 0019 | Fleet triggers the team's CI for one guarded rerun, and reads `--selection`; Fleet never runs tests, provides devices or owns a schedule | Supersedes spec §17 "CI orchestration or job scheduling" for dispatch only; 0015 stays rejected |
| 0020 | Shallow JUnit ingest (XCUITest, Espresso), written when W7 starts | Amends 0011 (Maestro only) and the ROADMAP "generic JUnit import" row |

0001–0008, 0010–0015 stay accepted; 0013 (planner) stays accepted, not marketed. The withdrawn pivot's ADR-0016 never merged; the number is free.

**Open issues**

| # | Fate | Reason |
|---|---|---|
| 211 | Rewrite: "Owner: hub decisions" | Keep google_apis 33, research/08, transcripts, immutable releases, Mac runner; add domains, Stripe, terms, hosting money, on-call, the "when" decision |
| 210 | Keep, Later | Sponsors |
| 209 | Keep, raise priority | Terms, privacy, DPA, delete-my-data policy before hosted launch |
| 208 | Keep, Later | Webhook as "post the link"; after the release page |
| 207 | Close, not planned | History lives in the server, which is the product |
| 206 | Rewrite, promoted (W3) | iOS `publish` from a Mac, capture study first |
| 205 | Rewrite | Validation plan (§7); gates billing, not engineering |
| 204 | Close, superseded | Hub release gate (new) |
| 203 | Rewrite | In-hub verdicts; no export |
| 202 | Keep (W8) | Labelled runs, accuracy.md |
| 201 | Keep (W4) | `fleet-junit.xml` for scripts and Datadog |
| 200 | Merge into the rerun issue | Tokenless fallback |
| 199 | Rewrite (W4) | Incidents on the last-night page and in the summary |
| 198 | Keep (W4) | Rule written before tuning; shared `fleet-core::diagnosis` |
| 197 | Keep (W4) | One device-lost rule |
| 196 | Keep (W4) | Infra capture study |
| 195 | Keep (W4) | Probes journalled so `publish` carries them |
| 194 | Keep (W1) | I1 also covers Maestro's files |
| 193 | Keep (W4) | `label.json`, versions |
| 192 | Keep (W4) | Blind check; banner also on the last-night page |
| 191 | Keep (W4) | maestro-watch |
| 188 | Rewrite (W0) | Tag alpha.4; drop the freeze steps |
| 178 | Keep, Later | Self-hosted pilots |
| 177 | Keep, Later | Pilots on managed Postgres |
| 172 | Promote (W1) | First load is the product for a captain opening a link cold |

**New issues (about 20), by milestone.** GitHub milestones "M7: The step" and "M8: Measured" close; their open issues move here. Each wave's completing PR flips a README "Status" row from "coming" to "available".

| Milestone | Issues (existing #, new N) | README row it flips |
|---|---|---|
| W0: alpha.4 and the hub | #188 (rewritten), N docs pass 1 (ADR-0016/17/18/19, README, description, PLAN, ROADMAP, milestones), #205 (rewritten), #211 (rewritten) | none (creates the table) |
| W1: Build key | N build key (`build.detected`), N builds table and release page, #194, #172 | "One record per app build" |
| W2: Hosted beta | N orgs and tenancy + `cross_org.rs`, N GitHub App sign-in and signup, N purge an org, N usage metering and quota banner, N hosted instance and restore drill, N hosted-ops.md, #209 (adds terms, privacy, DPA) | "Hosted (private beta)" |
| W3: iOS | #206 (rewritten) | "iOS Maestro" |
| W4: Last night | #191–#199, #201, N `CiContext` workflow and event, N last-night page | "Last night: incidents, infra label, blind banner" |
| W5: Quarantine and selection | N quarantine (ADR-0018), N `--selection` with path guards | "Quarantine", "Pick what runs" |
| W6: Rerun | N guarded rerun (ADR-0019, absorbs #200) | "Rerun from the page" |
| W7: Other frameworks | N shallow ingest XCUITest and Espresso (ADR-0020) | "XCUITest and Espresso results" |
| W8: Measured | #202, #203 (verdicts), N hub release gate (replaces #204), N Stripe (gated by #205) | "Measured accuracy", "Paid plans" |
| Later | #177, #178, #208, #210 | none |

---

## 6. Docs

| Document | Fate |
|---|---|
| README.md | Rewrite: the one line; "Self-hosted alpha today; no hosted tier yet. Android today; iOS next."; the five verbs as the feature list, each marked exists or coming; the measured section stays honest (research 15 setup cost, overhead) |
| spec-v1.0.md | Verbatim, behind a header listing superseded sections (§1, §11, §17). No spec-v2; criteria in PLAN §16 |
| PLAN.md | §1 → ADR-0016; §3 adds I2 (widened), I12, I13, I14; §12 hub milestones; §16 the hub's definition of done |
| ROADMAP.md | New table: M0–M6 done (M6 closed at alpha.4); hub milestones; close GitHub milestones "M7: The step" and "M8: Measured"; "Not planned" list in §1 |
| ADRs 0016–0020 | New, as above |
| user-guide | Release page, last night, quarantine, rerun, selection: one section per wave |
| operator-guide | "Hosted vs self-hosted"; new `docs/src/hosted-ops.md`: what runs where; how hosted differs (viewer tokens refused, per-project grants, 30-day retention); backups and the weekly restore drill with a dead-man switch; upgrades and migrations (append-only from W2); incidents (best effort, business hours, written into the terms; a status line); purge and export (speed of a delete-my-data request, audit row); secrets and rotation (GitHub App key, usercontent signing key, Stripe keys later); box access and its audit; cost and the quota banner |
| runner-guide | `FLEET_TOKEN`-only setup first; iOS publish when it ships |
| security.md | Tenancy threats: cross-org, purge completeness, the App's `actions: write`, the dispatch guard |
| CHANGELOG, AGENTS.md, CONTRIBUTING | AGENTS first line: "Fleet is a hub for mobile e2e tests (ADR-0016)"; one line each elsewhere |
| research | Unchanged; the two simulated transcripts (pivot, hub) published as research 16/17 with the "simulated, numbers invented" header only on the owner's call |
| handoff/withdrawn-pivot | Owner's call: outside history, or under research/ as a record |
| GitHub | Description as §1; topics keep android, ci, flaky-tests, github-actions, maestro, mobile-testing, rust, ui-testing, test-observability; add e2e-testing, test-management (ios once it ships); drop leptos, observability |

---

## 7. Business risks and owner decisions

**Validation (#205), starts now, no code needed.** Ten or more conversations with real teams; record the date, role, team size, frameworks, runners and platforms, never private names; ask about past behaviour only and do not pitch before question 5:
1. Walk me through your last release sign-off. Who did it, with which tools and pages, and how long did it take?
2. Your last red end-to-end run: what did you do in the first 30 minutes, and how did you decide whether it was the device or the app?
3. What have you built around your e2e tests (quarantine folders, Slack bots, rerun scripts)? Who keeps it working?
4. What do you pay today for test tooling and reporting (not salaries), and who approves that spend?
5. (Show the release page.) What would make you stop using this within a month, and what would you pay for it on a card? Then three free design partners on the hosted beta, measured on behaviour: the release captain posts the Fleet link instead of a Slack summary; triage minutes per red, before and after; quarantine entries that still have an owner after 30 days. No public price until three partners agree to one.

**Pricing.** Flat tiers by monthly flow results is the hypothesis (personas' estimates: $50–100, $300–500, ~$18k/yr audited; invented numbers). Arithmetic, not data: at $100–400 a month, a living takes tens of paying teams, so the hosted tier must be self-serve (token-only signup, card billing) before any enterprise feature.

**Owner decisions**
1. Merge #186 and tag alpha.4 now.
2. **Who owns "when"?** Decided by the owner on 2026-11-09: the team's CI cron decides when, and Fleet decides what (`--selection`). Fleet owns no schedule, so a Fleet outage cannot stop a run from starting (I2). The earlier option of an opt-in Fleet-owned schedule (about 2 days in W6) is removed.
3. Hosting money and setup: VM, S3, domains, GitHub App, Stripe, on-call. About $30–55 a month before Stripe fees (owner to confirm on vendor pages).
4. Legal before hosted launch (#209): terms, privacy, DPA, delete-my-data policy, plus the AGPL/commercial review.
5. The second maintainer: the CEO persona's six-month condition; a single maintainer running a multi-tenant service is the plan's biggest risk.
6. Mac time: iOS capture study, infra capture study, nightly labelled leg; install `google_apis` 33; install Gradle for the Espresso fixture.
7. Publish the simulated transcripts (research 16/17) or keep them private; the withdrawn-pivot handoff's place.
8. SOC 2 / EU residency: explicitly out of this horizon; the README says so.

**Risks**

| Risk | Mitigation |
|---|---|
| One maintainer running a hosted multi-tenant service | Private beta with three partners only; 30-day retention bounds data; runbook public; billing last; the second maintainer is an owner decision |
| Hosted sign-in tied to GitHub | Recorded limitation; OIDC stays for self-hosted; revisit after validation |
| Maestro ships its own classifier, or a runner-neutral rival arrives | The build key and cross-runner record are the entity nobody keeps alive as a script; the classifier says when it is blind |
| Validation says nobody pays | Billing is built last; engineering waves still produce a self-hosted hub the founder wanted |
| Cross-tenant leak | I14 e2e over every route; blobs keyed per project; purge test |
| The guarded rerun becomes a broad token | One UUID input, one workflow, CI OIDC auth, catalog-checked paths, audit |

---

## 8. Dissent, stated plainly

- **"When".** The docs engineer held that a Fleet-owned schedule breaks I2 (Fleet down means the run never starts); the founder's sentence asks for "when". Resolved by the owner on 2026-11-09: no Fleet-owned schedules. The team's CI cron owns "when", and `--selection` (W5) owns "what".
- **Hosted sign-in through GitHub only.** The server engineer's choice (the rerun needs the GitHub App anyway). It excludes GitLab/Bitrise-only teams from hosted sign-in and from dispatch. Accepted for the private beta; revisit after #205.
- **iOS priority.** The CTO persona calls iOS the product; the others would wait. Placed at W3, after the build key and the hosted tier, because both other personas start without it and the record needs the key first.
- **Where the moat is.** Cross-runner record (CEO persona) versus classifier and quarantine (platform-lead persona). Resolved by order, not by choice: the build key first, then both hang off it.
- **Removal versus freeze of the server** is moot: the server is the hub's foundation.
- **One maintainer.** Every engineer named it as the biggest risk. Not resolved by this plan; it is owner decision 5.
