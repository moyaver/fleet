# Fleet roadmap and milestone checklists

This document preserves the milestone checklist and acceptance evidence while the [README](../README.md) gives newcomers a short product introduction. The [implementation plan](PLAN.md#12-milestones) defines the milestone exit criteria. Status follows the code and CI evidence; plans are not shipped features.

## Roadmap

Each milestone has exit criteria in [PLAN §12](PLAN.md#12-milestones). A milestone starts only when the previous one's criteria pass or the owner waives them.
Issues are grouped by [GitHub milestone](https://github.com/moyaver/fleet/milestones).

| | Milestone | Scope | Exit criteria | Estimate |
|---|---|---|---|---|
| ✅ | [**M0** Bootstrap](https://github.com/moyaver/fleet/milestone/1) | Repo, licensing, workspace skeleton, xtask, CI, hello page | `cargo xtask ci` green locally and in CI; `fleet serve` shows the hello page; license files verbatim | 1–2 days |
| ✅ | [**M1** Phase 0: Maestro proof](https://github.com/moyaver/fleet/milestone/2) | Capture real Maestro 2.12.x runs on emulators as fixtures; answer the 17 open questions | Every progress state classified observable / inferred / unavailable with fixture evidence; device-correlation rule confirmed | 1–2 weeks |
| ✅ | [**M2** Agent](https://github.com/moyaver/fleet/milestone/3) | Parsers, supervisor/observer, metrics, adb, spool, transport, signed releases, run a planner shard | Invariants I1, I2, I5, I6, I10 proven by tests; < 1% CPU overhead; a real CI run streams unchanged | 3–5 weeks |
| ✅ | [**M3** Server core](https://github.com/moyaver/fleet/milestone/4) | Migrations, ingest, acks/resume, live hub, REST, OpenAPI, agent auth, Compose, plan storage | Concurrent agents don't corrupt state; restarts lose no committed events; acceptance criteria 3, 10, 11, 15 | 4–6 weeks |
| ✅ | [**M4** Dashboard core](https://github.com/moyaver/fleet/milestone/5) | `fleet-ui` kit, screens A–D, browser OIDC login, E2E | A developer can follow a full run without CI logs; acceptance criteria 2, 4, 5, 6, 14 | 4–6 weeks |
| ✅ | [**M5** Evidence and history](https://github.com/moyaver/fleet/milestone/6) | Artifacts, usercontent origin, screenshot and log viewers, fingerprints, flaky v1, screens E–F, **Fleet Planner** (duration-aware CI sharding, [ADR-0013](decisions/0013-fleet-planner.md)) | A failed run can be diagnosed after the runner is gone; acceptance criteria 7, 8, 9 | 3–4 weeks |
| 🚧 | [**M6** Hardening and v1.0.0](https://github.com/moyaver/fleet/milestone/7) | RBAC, admin screen, security review, load tests, runbooks, guides, release | All 15 acceptance criteria pass; IT can deploy from the docs alone | 3–4 weeks |
| ⬜ | [**M7+** Optional](https://github.com/moyaver/fleet/milestone/8) | Notifications, OTLP, GitLab/Buildkite, fork-PR relay, iOS, Leptos 0.9 | — | — |

✅ done · 🚧 in progress · ⏭️ next · ⬜ not started

### M0 checklist

- [x] Repository on the owner's personal account, private until the owner makes it public
- [x] Spec, plan, ADRs and research imported unchanged
- [x] `LICENSE` (GNU AGPL v3, verbatim; [ADR-0012](decisions/0012-agpl-license.md)), `LICENSING.md`, `CLA.md`, `CLA-CORPORATE.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1, verbatim), `SECURITY.md`, `FUNDING.yml`
- [x] Workspace skeleton: every crate compiles, SPDX headers, `publish = false`, `license`, workspace lints
- [x] `cargo xtask setup | ci | lint | web`
- [x] `deny.toml` and `about.toml`
- [x] `ci.yml` and `cla.yml`
- [x] Leptos hello page built by Trunk, embedded and served by `fleet serve` with the CSP
- [x] mdBook skeleton under `docs/`
- [x] GitHub settings: Discussions, merge options, topics, labels, milestones, `main` ruleset, private vulnerability reporting, secret scanning

### M1 checklist

- [x] Emulator capture setup with the latest Maestro (2.12.1), two emulators of one AVD
- [x] 24 scenarios captured as fixtures: pass, fail, suite, shards, `executionOrder`, retry, nested `runFlow`, signals, device loss, no device, bad flag, planted secrets, output paths, TTY/ANSI, adb load
- [x] `cargo xtask fixtures capture | import`
- [x] The 17 questions answered, ADR-0003 accepted ([research 08](../research/08-phase0-results.md))
- [x] Build vs buy: Allure 3 reads Maestro's failures as passes ([research 08 §5](../research/08-phase0-results.md#5-build-vs-buy-adr-0009)); ReportPortal not run (needs Docker)
- [ ] Post the upstream `--events` proposal (owner: draft in [research 08 §6](../research/08-phase0-results.md#6-upstream---events-reporter-open-decision-o3))

### M2 checklist

- [x] Supervisor: byte-exact passthrough, exit status and signal deaths unchanged, grace escalation, contained observer failures (I1)
- [x] `fleet-maestro` parsers (session log, stdout, bundles, JUnit), snapshot-tested on all 24 fixtures
- [x] Claim lattice (I3, I4, property-tested), redaction (I6), JUnit reconciliation
- [x] Live observer, CRC-framed spool, WebSocket transport with server-authoritative resume, `flush` (I2, I5)
- [x] Host, Maestro and agent metrics; adb device tracking without touching the adb server
- [x] `run`, `inspect`, `flush`, `check`, `version`; planner shards (`--plan`, ADR-0013)
- [x] Validated on a real emulator: unchanged result, 65 events gap-free, 0.14% of one core (I10)
- [x] Release pipeline (cargo-dist): musl x86_64/aarch64 + macOS arm64, checksums, attestations, third-party notices
- [x] HTTPS proxy support and OIDC token exchange (done in M3: [#16](https://github.com/moyaver/fleet/issues/16), [#17](https://github.com/moyaver/fleet/issues/17))

### M3 checklist

- [x] Migrations 0001–0005 on Postgres 18: tenancy, CI identity, execution (`CHECK`: inferred sources never claim passed), journal partitioned by `run_day`, telemetry; committed `.sqlx` data, `cargo xtask sqlx`
- [x] Ingest transaction: epoch fencing, duplicate skip, ownership-checked upserts, claims resolved by the lattice, suite and job outcomes (`fleet-core::outcome`), gaps recorded (I3, I5)
- [x] Agent WebSocket and HTTPS batch endpoints: acks, resume, fencing, keepalive, timeouts, `goaway` + 1012 on shutdown
- [x] `fleet serve | migrate | check-config | healthcheck | api-key create`, figment config (TOML, `FLEET_*`, `*_FILE`)
- [x] Live hub, browser WebSocket (snapshot + versioned deltas, `resync`), rebuilt from Postgres after a restart
- [x] REST snapshots, OpenAPI (utoipa), liveness sweeps under an advisory-lock leader
- [x] Agent auth: API keys (`mf_live_…`) and GitHub OIDC exchange with trust policies; the agent does the exchange ([#17](https://github.com/moyaver/fleet/issues/17))
- [x] Agent HTTPS proxy (CONNECT) and `FLEET_CA_FILE` ([#16](https://github.com/moyaver/fleet/issues/16))
- [x] Plan storage and planned totals ([#14](https://github.com/moyaver/fleet/issues/14))
- [x] Compose: Postgres 18, Garage, Caddy; cargo-chef + distroless image ([deploy/](../deploy/compose/README.md))
- [x] `fleet-loadgen` scenarios: 50 agents × 4 devices, reconnect storms, duplicates and reordering, server restart, overlapping job names
- [x] Exit criteria proven by tests: concurrent agents, restart without loss, acceptance criteria 3, 10, 11, 15, fixture replay through the real agent and server ([below](#m3-exit-criteria))

### M3 exit criteria

Each is an automated test that runs in CI (`cargo xtask ci`, Postgres 18). The integration-boundary hardening that had to land before M4 is tracked in [#30](https://github.com/moyaver/fleet/issues/30): all P0 and P1 findings except [#24](https://github.com/moyaver/fleet/issues/24) are fixed.

| Criterion | Proven by |
|---|---|
| Concurrent agents publish without corrupting state | `fleet-e2e` `scenarios::fifty_agents_with_four_devices_publish_without_corrupting_state` (50 agents × 4 devices, overlapping job names, resent and reordered batches: every event committed once, every flow under its own run, job and device); `fleet-store` `concurrent_runs_in_one_job_and_attempt_stay_consistent`, `a_new_run_never_deadlocks_with_a_batch_in_its_job` |
| A restart loses no committed event | `fleet-server` `a_restart_loses_no_committed_event`, `after_a_restart_the_hub_rebuilds_from_postgres`; `fleet-e2e` `a_server_restart_mid_run_loses_no_committed_event` |
| Acceptance 3: jobs and emulators stay separate | `fleet-e2e` `concurrent_jobs_and_same_serial_emulators_stay_separate` (two fixtures at once, one serial on both) and the 50-agent scenario |
| Acceptance 10: network interruptions don't fail tests | `fleet-e2e` `a_reconnect_storm_loses_nothing`, `a_refused_oidc_token_never_affects_maestro`; agent fail-open tests from M2 |
| Acceptance 11: the exit code is preserved | `fleet-e2e` `stored_outcomes_match_inspect_for_every_fixture` (12 fixtures, exit codes 0, 1, 130, 143) |
| Acceptance 15: failed and interrupted jobs never look successful | `fleet-core` `outcome` property tests; the DB `CHECK` and lattice tests (`inferred_sources_can_never_claim_passed`); `an_agent_that_never_returns_is_abandoned_and_its_job_never_green`; every fixture's job outcome |
| The real agent's stored outcomes match `fleet-agent inspect` | `fleet-e2e` `stored_outcomes_match_inspect_for_every_fixture` |
| I3, I5, I8 | DB `CHECK`s + claim lattice; `ingest_gaps` for every loss (asserted zero where nothing was lost); `xtask lint` keeps provider crates out (the OIDC verifier runs on `ring`) |

### M4 checklist

Exit criteria are proven by tests ([below](#m4-exit-criteria)). Tracking: [#47](https://github.com/moyaver/fleet/issues/47).

- [x] Live read models and frames shared by server and SPA (`fleet_protocol::live`), `LiveStore` with resync on a version gap ([19c380b](https://github.com/moyaver/fleet/commit/19c380b38eb1f267645a047cdeb48db682e0eaf6))
- [x] Browser sign-in: OIDC code + PKCE, Postgres sessions, CSRF, group → role mapping, `authorize` policy ([f3ec40c](https://github.com/moyaver/fleet/commit/f3ec40c7ea0625f4f80adaafcbc23008330665f3))
- [x] `fleet-ui` kit orders 1–8: tokens and themes (Tailwind standalone), layout, table, virtual list, provenance badges, overlays and forms, charts with LTTB, swimlane timeline, log viewer, step tree, screenshots ([b2ed1bf](https://github.com/moyaver/fleet/commit/b2ed1bf402464874d972b28dffdc01f6d2d8134d), [102b213](https://github.com/moyaver/fleet/commit/102b213b93970342dff64be9fba57ba7d4f7db60), [52fc584](https://github.com/moyaver/fleet/commit/52fc5842ee85d3a539e63460b75be950eb163995), [04735c4](https://github.com/moyaver/fleet/commit/04735c4e096af898e5d55a849f7a65d92a4cc337))
- [x] REST for screens A–D ([e99df65](https://github.com/moyaver/fleet/commit/e99df65695df8b099c8fa0869498a4074fbaf9e2)); screens A, B ([f48dbaf](https://github.com/moyaver/fleet/commit/f48dbaf3a0b5ef99e0d2840214db35b6ef0d4314)) and C, D ([6e7e1f1](https://github.com/moyaver/fleet/commit/6e7e1f17d7ba961a5837575cac914369542ca0f6))
- [x] Browser E2E harness (chromedriver + headless Chrome, zero CSP violations asserted) ([16129c1](https://github.com/moyaver/fleet/commit/16129c1d4ced91e2d9a96fa7e2f8eebcac77b2df)) and the M4 exit suite ([eee9e1c](https://github.com/moyaver/fleet/commit/eee9e1cc2bbd538c8709b8c699b83cea2021ea3f))
- [x] Agent HTTPS batch fallback when WebSocket upgrades keep failing ([#31](https://github.com/moyaver/fleet/issues/31), landed in [#65](https://github.com/moyaver/fleet/pull/65))
- [x] Generated protocol JSON Schema and golden frame tests ([#36](https://github.com/moyaver/fleet/issues/36), landed in [#65](https://github.com/moyaver/fleet/pull/65))

### M4 exit criteria

Browser tests against the real server and Postgres 18, in `crates/fleet-e2e/tests/it/` (run by `cargo xtask ci`).

| Criterion | Proven by |
|---|---|
| Follow a full run without CI logs (Phase 3 gate) | `m4_exit::ac02_a_real_agents_failing_run_is_followed_live_from_the_overview_to_the_failed_flow` (real agent, captured fixture: overview → pipeline → failed flow, no reload) |
| 2. The browser updates during execution, without manual refresh | the walkthrough above; `dashboard_ab::a_run_appears_on_the_overview_and_its_counts_move_without_a_reload`, `dashboard_ab::after_a_server_restart_the_page_resubscribes_and_keeps_updating` |
| 4. Host and device metrics have accurate labels and units | `dashboard_cd::the_worker_page_labels_metrics_with_units_and_shows_missing_thermal_as_unavailable` |
| 5. Missing thermal sensors show unavailable, never fake values | the same test; `dashboard_cd::a_device_page_lists_its_flows_with_links_and_shows_missing_sensors_as_unavailable` |
| 6. Flow progress is accurate or explicitly marked uncertain | `m4_exit::ac06_progress_counts_toward_the_planned_total_while_the_run_goes`, `m4_exit::ac06_a_real_agent_running_a_plan_shows_its_count_while_maestro_runs`; `dashboard_ab::pipeline_detail_shows_flows_with_provenance_and_says_when_progress_is_uncertain` |
| 14. Usable with multiple simultaneous active runs | `m4_exit::ac14_ten_simultaneous_runs_stay_correct_and_the_page_stays_responsive` |
| 12. Private test data is protected by company authentication | `m4_exit::ac12_signing_in_with_the_company_identity_provider_works_in_the_browser` and the sign-in HTTP tests in `fleet-server` |

### M5 checklist

- [x] Failure and condition fingerprints, flaky v1 (ADR-0009), migration 0007 ([bef75c4](https://github.com/moyaver/fleet/commit/bef75c48a4f9c6b2b3c6c5c6289696af64af5d8d), [a03dd06](https://github.com/moyaver/fleet/commit/a03dd063d26dc452456b531b9fb02eeee33d7076)); flaky v1 thresholds decided in [#44](https://github.com/moyaver/fleet/issues/44) and set by [#65](https://github.com/moyaver/fleet/pull/65): at least five eligible default-branch runs, flip rate ≥ 0.2 over the latest 20 ([ADR-0009](decisions/0009-scope-focused-build.md))
- [x] `BlobStore` (filesystem and S3/Garage, one contract suite) and artifact verification (magic bytes, zip/tar.zst safety) ([01775c4](https://github.com/moyaver/fleet/commit/01775c4799ae8e4269bf79f4144d9b8230338f9a))
- [x] Artifact upload protocol, agent uploader (redacted, fail-open), usercontent origin with signed links and `CSP: sandbox` (I7), migration 0008 ([db4eb13](https://github.com/moyaver/fleet/commit/db4eb130a020611104957611bfc95cfb744fb747))
- [x] Retention, rollups (1 min, 1 h), partition drops, failure-evidence promotion, blob GC with a grace period, migration 0009 ([587df25](https://github.com/moyaver/fleet/commit/587df250e146fdeb995dae27bcbee1262fbb3e50), closes [#32](https://github.com/moyaver/fleet/issues/32))
- [x] Screens E (History) and F (Compare) with evidence-only mismatch explanations ([f127d20](https://github.com/moyaver/fleet/commit/f127d209fedb59f6a04a01e94f9d5b19a2dd8c67))
- [x] Evidence on screen D (screenshots, log at the failing step, step timings, sandboxed HTML report) and the M5 exit suite ([b64e7db](https://github.com/moyaver/fleet/commit/b64e7db81bd81e771eb7106753c930b635bf786c))
- [x] **Fleet Planner**: duration-aware CI sharding ([#15](https://github.com/moyaver/fleet/issues/15), [#69](https://github.com/moyaver/fleet/pull/69), [ADR-0013](decisions/0013-fleet-planner.md)): `fleet-planner plan` writes the manifest and matrix and publishes the plan; `fleet-agent run --plan` runs one shard with the workspace config

### M5 exit criteria

| Criterion | Proven by |
|---|---|
| 7. Final results reconcile with Maestro's JUnit output | `m5_exit::ac07_final_outcomes_equal_the_junit_report` |
| 8. Failures link to actionable debugging evidence | `m5_exit::ac08_a_failed_flow_links_its_screenshot_and_log_lines` |
| 9. Test history remains accessible after the runner disappears | `m5_exit::ac09_history_and_evidence_outlive_the_runner_and_a_server_restart` |
| I7. User-uploaded HTML never runs on the app origin | `m5_exit::i7_an_uploaded_html_report_never_runs_on_the_app_origin` |

### M6 checklist

Tracking: [#86](https://github.com/moyaver/fleet/issues/86). Items from the [2026-11-06 competitive review](../research/09-competitive-review-2026-11.md) are included.

- [ ] Correct results: nested steps and retry attempts (#134), the post-exit drain of a backed-up CI log (#128), per-run runner metrics (#129), Maestro version per run (migration 0013). Open: infrastructure exclusions in flaky analysis
- [ ] Evidence Maestro already writes: missing fixtures (#130), the failing step's UI hierarchy (#132), a bounded logcat excerpt and `app.crashed` (#131), device samples (#136). Open: Maestro's `takeScreenshot` images and recordings (#133), and an ANR fixture (#77; not captured)
- [ ] Install and operate: composite GitHub Action with a job summary (#125), the server image for amd64 and arm64, backup and restore, and a free-VM guide ([Try Fleet on a free VM](src/free-vm.md)). Open: a release whose agent includes the job summary, which `v0.1.0-alpha.1` does not
- [ ] Hardening: rate limits for pre-auth, agent and viewer requests (#118, #135), a bounded journal (`FLEET_JOURNAL_MAX_BYTES`). Open: multi-instance fan-out, remaining agent review items, accessibility
- [ ] Proof: nightly real-Maestro job on the owner's Mac (#127), repeatable overhead benchmark (`cargo xtask bench-overhead`), comparative diagnosis study ([research 10](../research/10-comparative-diagnosis-study.md), #137), planner planned vs actual

### Owner actions

Things only the owner can do. Each one has an issue labelled [`owner-action`](https://github.com/moyaver/fleet/labels/owner-action).

- [x] **Before product code:** written confirmation from the employer that this is personal IP ([#1](https://github.com/moyaver/fleet/issues/1), [PLAN §2.5](PLAN.md#25-precondition-employer-ip-check-owner-action-before-m0-code))
- [x] Decide the final product name: **Fleet** ([#5](https://github.com/moyaver/fleet/issues/5), [ADR-0011](decisions/0011-name-and-runner-scope.md))
- [ ] Ask a lawyer to review the AGPL + commercial-license model, the CLA adaptations and the employer letter ([#6](https://github.com/moyaver/fleet/issues/6))
- [x] Add a public contact email for Code of Conduct reports and Corporate CLA delivery ([#2](https://github.com/moyaver/fleet/issues/2))
- [x] Decide the flaky v1 thresholds ([#44](https://github.com/moyaver/fleet/issues/44), landed in [#65](https://github.com/moyaver/fleet/pull/65))
- [ ] Set up a GitHub Sponsors profile ([#3](https://github.com/moyaver/fleet/issues/3))
- [x] When public: branch protection on `main`, private vulnerability reporting, secret scanning ([#4](https://github.com/moyaver/fleet/issues/4))

---
