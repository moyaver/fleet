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
| ⏭️ | [**M4** Dashboard core](https://github.com/moyaver/fleet/milestone/5) | `fleet-ui` kit, screens A–D, browser OIDC login, E2E | A developer can follow a full run without CI logs; acceptance criteria 2, 4, 5, 6, 14 | 4–6 weeks |
| ⬜ | [**M5** Evidence and history](https://github.com/moyaver/fleet/milestone/6) | Artifacts, usercontent origin, screenshot and log viewers, fingerprints, flaky v1, screens E–F, **Fleet Planner** (duration-aware CI sharding, [ADR-0013](decisions/0013-fleet-planner.md)) | A failed run can be diagnosed after the runner is gone; acceptance criteria 7, 8, 9 | 3–4 weeks |
| ⬜ | [**M6** Hardening and v1.0.0](https://github.com/moyaver/fleet/milestone/7) | RBAC, admin screen, security review, load tests, runbooks, guides, release | All 15 acceptance criteria pass; IT can deploy from the docs alone | 3–4 weeks |
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
- [x] HTTPS proxy support and OIDC token exchange (done in M3: [#24](https://github.com/moyaver/fleet/issues/24), [#25](https://github.com/moyaver/fleet/issues/25))

### M3 checklist

- [x] Migrations 0001–0005 on Postgres 18: tenancy, CI identity, execution (`CHECK`: inferred sources never claim passed), journal partitioned by `run_day`, telemetry; committed `.sqlx` data, `cargo xtask sqlx`
- [x] Ingest transaction: epoch fencing, duplicate skip, ownership-checked upserts, claims resolved by the lattice, suite and job outcomes (`fleet-core::outcome`), gaps recorded (I3, I5)
- [x] Agent WebSocket and HTTPS batch endpoints: acks, resume, fencing, keepalive, timeouts, `goaway` + 1012 on shutdown
- [x] `fleet serve | migrate | check-config | healthcheck | api-key create`, figment config (TOML, `FLEET_*`, `*_FILE`)
- [x] Live hub, browser WebSocket (snapshot + versioned deltas, `resync`), rebuilt from Postgres after a restart
- [x] REST snapshots, OpenAPI (utoipa), liveness sweeps under an advisory-lock leader
- [x] Agent auth: API keys (`mf_live_…`) and GitHub OIDC exchange with trust policies; the agent does the exchange ([#25](https://github.com/moyaver/fleet/issues/25))
- [x] Agent HTTPS proxy (CONNECT) and `FLEET_CA_FILE` ([#24](https://github.com/moyaver/fleet/issues/24))
- [x] Plan storage and planned totals ([#21](https://github.com/moyaver/fleet/issues/21))
- [x] Compose: Postgres 18, Garage, Caddy; cargo-chef + distroless image ([deploy/](../deploy/compose/README.md))
- [x] `fleet-loadgen` scenarios: 50 agents × 4 devices, reconnect storms, duplicates and reordering, server restart, overlapping job names
- [x] Exit criteria proven by tests: concurrent agents, restart without loss, acceptance criteria 3, 10, 11, 15, fixture replay through the real agent and server ([below](#m3-exit-criteria))

### M3 exit criteria

Each is an automated test that runs in CI (`cargo xtask ci`, Postgres 18). Next, before M4 builds on the agent's evidence: the integration-boundary hardening tracked in [#39](https://github.com/moyaver/fleet/issues/39).

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

### Owner actions

Things only the owner can do. Each one has an issue labelled [`owner-action`](https://github.com/moyaver/fleet/labels/owner-action).

- [x] **Before product code:** written confirmation from the employer that this is personal IP ([#1](https://github.com/moyaver/fleet/issues/1), [PLAN §2.5](PLAN.md#25-precondition-employer-ip-check-owner-action-before-m0-code))
- [x] Decide the final product name: **Fleet** ([#5](https://github.com/moyaver/fleet/issues/5), [ADR-0011](decisions/0011-name-and-runner-scope.md))
- [ ] Ask a lawyer to review the AGPL + commercial-license model, the CLA adaptations and the employer letter ([#6](https://github.com/moyaver/fleet/issues/6))
- [x] Add a public contact email for Code of Conduct reports and Corporate CLA delivery ([#2](https://github.com/moyaver/fleet/issues/2))
- [ ] Set up a GitHub Sponsors profile ([#3](https://github.com/moyaver/fleet/issues/3))
- [x] When public: branch protection on `main`, private vulnerability reporting, secret scanning ([#4](https://github.com/moyaver/fleet/issues/4))

---
