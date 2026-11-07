# 12: Positioning and borrowing

**Research date:** 2026-11-07
**Scope:** Whether Fleet's architecture, scope and hosting model fit the people who would use it, and what Fleet should borrow from Marathon and Flank. Repo facts were checked in the working tree on `main` (commit `fac7343`). Marathon and Flank facts are carried from research 11 and were not re-checked here.
**Builds on:** [07 Prior art and build-vs-buy](07-prior-art-and-build-vs-buy.md), [09 Competitive review](09-competitive-review-2026-11.md), [11 Marathon and Flank as prior art](11-marathon-and-flank.md), [ADR-0013](../docs/decisions/0013-fleet-planner.md)

## How to read this report

Markers follow research 09 and 11:

- **[V]**: verified today against a primary source. The repo path with line, or the URL, is given.
- **[R11]**: carried from research 11 and not re-checked in this note. Section numbers are given.
- **[I]**: inferred, or opinion. Treat as a hypothesis.

Most of this note is judgement, so most items are [I].

---

## 1. Bottom line

1. The architecture is heavier than a single-run report needs. A report written by the agent into a CI artifact would be enough. The server and Postgres pay for themselves only through a live view across many runners and cross-run history. [I]
2. The useful niche is narrow. It needs dozens to hundreds of Maestro flows, several machines the company owns, real time lost to flaky failures, and someone whose job is triage. Below that, JUnit plus CI artifacts covers most of the need. [I]
3. Stay self-hosted. A hosted version creates tenancy, billing and hosting work, and it does not create demand. [I]
4. Automated Test Device (ATD) images fit Maestro badly. Google's own constraints rule out hardware rendering and remove system UI and the keyboard. On Linux, use normal `google_apis` images with software rendering. [V for the constraints, I for the recommendation]
5. History is useful for two things: telling flaky from broken, and grouping red runs by cause. Trend charts and long retention are not. Step-level grouping is where Fleet can beat JUnit-only flaky tools. [I]
6. The larger opportunity is a dynamic flow queue across CI jobs that have already launched. It reverses the "Static only" rule in ADR-0013 and conflicts with spec §17. It needs an owner decision and a new ADR, and the server must stay off the critical path (I2). [I]
7. Today Fleet is worse than plain CI for "why did the app crash". `app.crashed` is journalled but not projected. Fix that before claiming the dashboard helps triage. [V]
8. Worth borrowing from Marathon: the dynamic queue, retries limited to infrastructure failures, a "runs needed for confidence" figure shown instead of run, and a per-device timeline. Flank's lessons (unknown-test estimates, no empty shards, no merged-JUnit history, no build to list tests) are already handled by the planner. [I, §8 and §9]

---

## 2. Is the architecture normal?

A single-run report needs no server. The agent could write an HTML file, and CI would keep it as an artifact. The server and Postgres exist for two things only: a live view across many runners, and cross-run history. Remove both and the server goes. [I]

Comparable self-hosted test-reporting servers are heavier. ReportPortal's official `docker-compose.yml` (fetched 2026-11-07 from `https://raw.githubusercontent.com/reportportal/reportportal/master/docker-compose.yml`) defines these services [V]:

| Group | Services (image) |
|---|---|
| Edge | `gateway` (traefik v2.11.60) |
| Data stores | `postgres` (18.6), `rabbitmq` (4.4.0), `opensearch` (3.9.0) |
| ReportPortal core | `migrations`, `index`, `ui`, `api`, `uat`, `jobs` |
| Auto-analysis | `analyzer-storage-init` (busybox), `analyzer` |

That is 12 services and three stateful stores. Object storage is a named volume called `storage` in the `volumes:` section, not a separate service [V, same file, `volumes:` at line 759].

Fleet's stack is smaller. PLAN §5.5 names PostgreSQL 18 as the database, object storage as optional (Garage, SeaweedFS, any S3, or a built-in filesystem), and a reverse proxy [V, `docs/PLAN.md:270-276`]. Each runner also runs the agent. [V, `docs/spec-v1.0.md:95`]

Conclusion [I]: Fleet's server is not an outlier for this niche. ReportPortal is heavier. The cost question is whether the live view and history justify even one server and one Postgres, and that depends on the niche in §3.

Not verified: ReportPortal's production deployment guide, Helm chart, or hardware sizing. The compose file is a development-oriented reference, so it may not match what the project recommends for production.

---

## 3. Who it is useful for

Useful when all of these hold [I]:

- Dozens to hundreds of Maestro flows.
- Spread across several machines or emulator hosts the company owns. Maestro Cloud and Firebase are out of scope.
- Flaky failures cost real time every week.
- Someone's job is triage, and they will look at a dashboard.

Below that, Maestro JUnit output, CI artifacts and perhaps Allure cover about 80% of the need [I; estimate, not measured].

Three shapes of use [I]:

- **One Linux machine.** Maestro's `--shard-split` already spreads flows across that machine's emulators [V, `docs/decisions/0013-fleet-planner.md:6`]. Fleet adds flaky history, step timings, and host and device health. That is worth it only if flakiness costs real time.
- **Many machines.** Fleet contributes the planner, which splits the suite across CI jobs by past durations, and one combined view. CI still launches each job [V, `docs/decisions/0013-fleet-planner.md:9-14`].
- **Triage at scale.** This is the case the product is built for: a failed run should tell you whether it is flaky or broken, and which cause it shares with other red runs. §6 covers this.

**The honest current gap: "why did the app crash".** The agent journals `app.crashed` when Maestro's `crash-report.txt` shows a crash [V, `crates/fleet-agent/src/session.rs:783`]. The store keeps the event in the journal and does not project it. The comment in `apply.rs` reads "Kept in the journal; projected by later milestones (artifacts: M5)" [V, `crates/fleet-store/src/ingest/apply.rs:180-185`]. The crash report itself is not uploaded. Research 09 §4.1 lists crash and ANR reports as "Ignored" and notes that no fixture captures them [V, `research/09-competitive-review-2026-11.md`, §4.1 table, line 81]. Issue #152 tracks the fix.

Validation test [I]: run Fleet on one real app's Maestro suite in CI for two weeks. If the team opens the dashboard instead of the CI log when something fails, continue. If they do not, and you cannot name three teams running self-hosted Maestro at scale who would, stop, or shrink the product to a single-binary CLI report.

---

## 4. Self-hosted vs SaaS

- A hosted version lowers trial friction. It does not create demand. [I]
- The shape fits self-hosting. The agent opens an outbound WebSocket [V, `docs/spec-v1.0.md:51` "Publish events over an outbound WebSocket"; `:95` "Agent establishes authenticated WebSocket"]. Runners point at a URL and need no inbound connection [I, from the outbound design].
- The licence supports a paid hosted version and blocks closed rehosting. ADR-0012 sets `AGPL-3.0-only`, with "No closed forks, no closed SaaS variants", and keeps the CLAs so the owner can dual-license [V, `docs/decisions/0012-agpl-license.md:9-13`]. `CLA.md` and `CLA-CORPORATE.md` exist at the repo root [V, `ls CLA*`]. [I] on the commercial reading.
- The niche works against SaaS [I]:
  - The best-fit customers run their own device farms, often because builds, screenshots and logs must not leave their network. Redaction and export policy (I6) is their concern.
  - Teams that are happy with SaaS already use Maestro Cloud, BrowserStack or Firebase dashboards.
  - A hosted service adds tenant isolation, billing, blob storage cost, uptime, and SOC 2 questionnaires.
  - Flaky detection from JUnit history already exists in Datadog Test Optimization, Buildkite Test Engine and Trunk [I; not checked against their current docs].
- Schema check. `migrations/0001_tenancy.sql` defines `projects` (line 19), `users` (line 30) and `role_bindings` (line 43). The `role_bindings` comment reads "A user's role on one project (NULL = every project)" (line 42). There is no organisation or tenant table above `projects` [V; `grep -i 'organi|tenant' migrations/` returns nothing]. A multi-customer SaaS would need a tenant layer that does not exist yet [I].
- Note for the owner: `docs/PLAN.md:54` still describes the PolyForm licence. ADR-0012 supersedes ADR-0010 and sets AGPL. The PLAN text is stale [V]. Not changed here.

Recommendation [I]: stay self-hosted. Run one hosted demo instance for evaluation only. Build a SaaS only if demo users ask to be hosted.

---

## 5. Emulator images: ATD is a poor fit for Maestro

Google's Gradle Managed Devices page [V, `https://developer.android.com/studio/test/gradle-managed-devices`, fetched 2026-11-07] says:

- ATDs "disable hardware rendering" as part of their optimisation. The page warns: "Screenshot tests that depend on hardware rendering currently aren't supported when using ATDs".
- "ATDs currently support only API level 30."
- The ATD image removes components including SystemUI and LatinIME (the AOSP keyboard) [V, same page, "What's removed in ATD images"]. "AOSP keyboard" is [I].

So ATD suits Espresso instrumentation tests, which run inside the app process. It fits Maestro's black-box UI tests less well, because those depend on system dialogs, the keyboard and the normal rendering path [I]. The Maestro flows that type into fields or dismiss system UI were not tested on ATD.

Marathon Cloud runs Maestro on `google_apis` system images for Android [R11 §2.1]. On Linux, Fleet should use normal `google_apis` images, headless, with software rendering [I].

---

## 6. How useful is history?

| Tier | What it answers | Evidence | Verdict |
|---|---|---|---|
| Useful | Flaky or broken. "Fails 1 in 12 on all commits" vs "failing every run since commit X". | Flakiness is *confirmed* (pass and fail on the same commit under the same condition) or *suspected* (at least five eligible runs, flip rate of 0.2 or more over the latest 20). Infrastructure failures are excluded [V, `docs/decisions/0009-scope-focused-build.md:15`]. | Keep. This is the core of the product. |
| Useful | "These 40 red runs are 2 problems." Grouping by cause. | `failure_fingerprints` (`migrations/0007_analytics.sql:25`, index at line 33) and `condition_fingerprints` (line 9). Written by `crates/fleet-store/src/ingest/analytics.rs:143` [V]. | Keep. This is where Fleet beats JUnit-only tools. |
| Small | Durations for splitting work. | Planner uses per-flow p50, with p90 as tie-break [V, `crates/fleet-planner/src/main.rs:135-136`]. | Beats a count split by roughly 10 to 30% of wall time [I; estimate, not measured]. Matters less once work is handed out dynamically. |
| Mostly useless | Trend charts that nobody opens. Retention beyond 2 to 4 weeks. | None. | Drop or defer. [I] |

Conclusion [I]: history is a means. The product is a one-line answer on the failed run, for example "flaky: failed 3 of the last 40 runs, same step". Step-level and fingerprint grouping is where Fleet can beat tools that only read JUnit.

`device_sessions` (`migrations/0003_execution.sql:20`) and `shards` (line 61) hold the data for a per-device timeline, which §8 uses.

---

## 7. Orchestration: the bigger opportunity

- Marathon hands the next test to whichever device is free. Fleet's planner is static. One slow job still holds up the pipeline [R11 §2.3; V, `docs/decisions/0013-fleet-planner.md:17`].
- Nobody offers self-hosted Maestro across a device pool. Marathon's open-source runner and Flank do not run Maestro. Marathon Cloud and Maestro Cloud run on their own devices [R11 §1, §5.3].
- It reverses scope. Spec §17 excludes "CI orchestration or job scheduling" (`docs/spec-v1.0.md:1187`). ADR-0013 says "Static only. A dynamic work queue (leases, idle runners pulling work) is orchestration and stays out of v1" (`docs/decisions/0013-fleet-planner.md:17`).
- Hard constraint, I2: "Fleet misconfiguration or outage never prevents or fails a Maestro run" [V, `docs/PLAN.md:129`]. If runners pull work from the server, the server is on the critical path. Any design must fall back to the static plan or to plain `--shard-split` when the server is unreachable [I].
- The planner already follows this rule for history. A failed history fetch becomes a warning and a fallback to supplied estimates or an even split [V, `crates/fleet-planner/src/main.rs:182-184`]. A failed publish is a warning too [V, `main.rs:238-245`].

Two stages [I]:

1. **Dynamic flow queue across CI jobs CI already launched.** Each job asks the server for its next flow until the queue is empty. This fixes the slowest-shard problem without Fleet owning any device. Static manifests stay as the fallback.
2. **Device registry.** Fleet owns the devices: reboots, wiping, installs. That is the step where Fleet becomes a full runner. Defer it, and only if stage 1 proves the demand.

This needs an ADR superseding the relevant part of ADR-0013 and spec §17. That is an owner decision.

---

## 8. What to borrow from Marathon

Facts are from research 11 §2 [R11]. Fleet references are checked in this repo.

| Idea | Marathon | Fleet adaptation | Conflict / cost |
|---|---|---|---|
| 1. Dynamic per-device queue | A free device pulls the next test [R11 §2.3] | Stage 1 in §7: each CI job pulls its next flow from the server | Reverses ADR-0013 "Static only" and spec §17. Needs I2 fallback to the static plan. |
| 2. Longest-first from history | Execution-time percentile sort [R11 §2.3] | Planner already sorts longest-first and assigns to the least-loaded shard (`crates/fleet-planner/src/main.rs:491-496`, `:519`). Reuse the same order inside a dynamic queue. | None beyond item 1. |
| 3. Device pools by model, OS, ABI | Pooling strategies [R11 §2.3] | Only meaningful with a device registry (§7 stage 2) | Needs device ownership, which Fleet does not have. |
| 4. Retry quotas | Post-factum retries with a quota per run [R11 §2.3] | If retries are ever allowed, cap total retries per run. Show retried-then-passed as flaky, never plain passed. | Spec §17 excludes automatic failure retries (`docs/spec-v1.0.md:1192`). I3 forbids a false pass (`docs/PLAN.md:130`). |
| 5. Uncompleted retries | Re-run only after a device crash or timeout [R11 §2.6] | Retry only infrastructure failures, which Fleet already separates in flakiness (`docs/decisions/0009-scope-focused-build.md:15`). This is the most defensible retry, since it does not mask test failures. | Still conflicts with spec §17. Needs an explicit exception. |
| 6. Preventive retry probability | Extra runs to reach a target pass probability, for example 0.5 becomes 3 runs for 0.8 [R11 §2.3] | Compute and show "this flow needs N runs to reach 95% confidence" from history, without running anything. | Stays inside spec §17 because nothing is re-run. [I] |
| 7. Per-device timeline | Timeline report [R11 §2.4] | Per-device Gantt from `device_sessions` (`migrations/0003_execution.sql:20`) and `shards` (line 61). It would show idle devices and the slow shard. | Data exists. Needs a new screen. |
| 8. Per-test video and screenshots | Video and screenshots per test [R11 §2.4] | Fleet does not upload these today. Screenshots are parsed but the step link is unused, and video is ignored [V, research 09 §4.1 table]. | I6 requires authorized, expiring access and redaction. Research 09 says video "can't be redacted at all", so video conflicts with I6 as written. |

---

## 9. What to borrow from Flank

Facts are from research 11 §3 [R11]. Fleet references are checked in this repo.

| Idea | Flank | Fleet status | Conflict / cost |
|---|---|---|---|
| 1. Estimate for unknown tests | Smart Flank estimates unknown tests at 10 seconds [R11 §3.3] | Already handled, without a constant. Unknown flows get the median of the known estimates. If no flow has any, every flow weighs 1 second, which is an even split [V, `crates/fleet-planner/src/main.rs:434-449`, fallback at line 439]. | None. Possible improvement: a per-workspace default in config. [I] |
| 2. Empty shards | Uniform sharding is "not recommended" because empty shards fail [R11 §3.3] | Fleet refuses to emit an empty shard. It fails the plan with "lower --shards" [V, `main.rs:526-531`; design note at `main.rs:11-12`]. | Not a gap. Cost: a `--shards` value above the unit count fails the planning job. Option [I]: clamp the count and warn, so the matrix is never empty. |
| 3. Full JUnit result for flaky tests | Adds failure nodes to passed flaky tests so CI tools see flakiness [R11 §3.3] | Fleet must not change Maestro's own output (I1, `docs/PLAN.md:128`). Option [I]: write a separate enriched JUnit file for CI tools, and leave Maestro's file as is. | Extra artifact to produce and keep in step with the store. |
| 4. Merged JUnit file as history | Smart Flank merges XML files, which its own design doc calls "complicated" [R11 §3.3] | Fleet keeps per-flow stats in Postgres. The planner reads them from the stats endpoint [V, `main.rs:546-570`]. | Lesson already applied. |
| 5. Enumerate tests without a build | Hypershard lists tests without a build [R11 §3.2] | Already done. The planner reads each candidate flow's YAML header directly [V, `main.rs:252-253` and `read_header` at `main.rs:307`]. | None. |
| 6. Fail-fast | Limits Firebase's automatic re-attempts after suspected infrastructure problems [R11 §3.3] | If Fleet ever adds infrastructure-only retries (§8 item 5), give teams the same off switch. | None until retries exist. [I] |

One cost to note in the planner [V, `main.rs:546-570`]: `fetch_history` makes one HTTP request per flow, each with a 15-second timeout (`main.rs:32`). That is fine for tens of flows and slow for hundreds. A batch stats endpoint would fix it [I].

---

## 10. Not verified

- Marathon and Flank facts are from research 11 and were not re-checked here. No clone or fetch was made for this note.
- No live runs of Marathon or Flank, and no live Fleet run for this note.
- ReportPortal: only the public `docker-compose.yml` was read. Its production deployment, Helm chart and sizing were not checked.
- ATD: the page was fetched on 2026-11-07. Quotes were checked against that text. The SystemUI and LatinIME entries were read from the removed-components table. "AOSP keyboard" is [I].
- Whether Maestro's text entry depends on the stock keyboard was not tested.
- The percentages (80% of need covered by JUnit tooling, 10 to 30% of wall time from durations, the 95% confidence example) are estimates, not measurements.
- Research 11 §4 says "Quarantine: No (ADR-0009)". ADR-0009 does not use the word; it excludes "test management" (line 18), and `docs/ROADMAP.md:163` maps quarantine to that exclusion. The citation is indirect but supported.
- Competitor tools named in §4 (Datadog Test Optimization, Buildkite Test Engine, Trunk) were not checked against their current docs.
- `docs/PLAN.md:54` still describes the superseded PolyForm licence. Noted in §4, not changed.

---

## Sources

**Repository files (read on `main`, `fac7343`)**
- `docs/decisions/0013-fleet-planner.md` (lines 6, 9-14, 15, 17, 21-23)
- `docs/decisions/0009-scope-focused-build.md` (line 15)
- `docs/decisions/0012-agpl-license.md` (lines 9-13)
- `docs/spec-v1.0.md` (lines 51, 95, 1182-1192)
- `docs/PLAN.md` (lines 54, 128-130, 270-276)
- `crates/fleet-planner/src/main.rs` (lines 4-12, 135-136, 182-184, 238-245, 252-254, 307, 434-449, 472-473, 491-496, 519, 526-531, 546-570)
- `crates/fleet-core/src/plan.rs` (lines 6-17, the `executionOrder` unit and equal-count baseline)
- `crates/fleet-store/src/ingest/apply.rs` (lines 180-185)
- `crates/fleet-store/src/ingest/analytics.rs` (line 143)
- `crates/fleet-agent/src/session.rs` (line 783)
- `migrations/0001_tenancy.sql` (lines 19, 30, 42-43)
- `migrations/0002_ci_identity.sql` (line 77, `plan_shards`)
- `migrations/0003_execution.sql` (lines 20, 61)
- `migrations/0007_analytics.sql` (lines 9, 25, 33)
- `research/09-competitive-review-2026-11.md` (§4.1 table, line 81)
- `research/11-marathon-and-flank.md` (§1, §2.1-2.6, §3.2-3.3, §4, §5.3)
- `CLA.md`, `CLA-CORPORATE.md` (repo root)

**External**
- ReportPortal compose: `https://raw.githubusercontent.com/reportportal/reportportal/master/docker-compose.yml` (fetched 2026-11-07)
- Android Gradle Managed Devices, ATD section: `https://developer.android.com/studio/test/gradle-managed-devices` (fetched 2026-11-07)
- Marathon and Flank: as listed in research 11 §Sources [R11]
