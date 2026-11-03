# 00 — Research synthesis: Maestro Fleet in Rust

**Date:** 2026-11-03 · **Input:** [spec v1.0](../docs/spec-v1.0.md) plus seven research reports (01–07)  
> **Update (same day):** the owner chose **all Rust including the dashboard** (Leptos CSR, [ADR-0004](../docs/decisions/0004-frontend.md)) and **source-available licensing** ([ADR-0010](../docs/decisions/0010-licensing.md)). The executable plan is [docs/PLAN.md](../docs/PLAN.md). Sections 5 and 10 below record the analysis as it stood before those decisions.

**Bottom line:** the plan is sound, and Rust fits the **agent** and **server** well. Research changed four things that matter:

1. **Console scraping can't deliver the live view the spec promises.** The live source has to be Maestro's session `maestro.log` plus its per-flow artifact folders (§2).
2. **The agent needs a two-process design** so a telemetry bug can never change a CI result (§3).
3. **The wire protocol needs a stricter identity and ack model** than spec §5 describes (§4).
4. **The frontend is the one place where "all Rust" costs much more than it gains** (§5). This is the decision that is still open.

Evidence labels used across the reports: **[V]** verified against a primary source (code, docs, measurement), **[M]** measured in a throwaway PoC, **[D]** vendor docs, **[I]** inferred, **[P]** design proposal. Facts that change over time (versions, product status, dates) were checked on 2026-11-03 and should be rechecked before relying on them.

| # | Report | Scope |
|---|---|---|
| 01 | [Maestro CLI internals](01-maestro-cli-internals.md) | What Maestro 2.12.1 really prints and writes, and when; read from the Kotlin source |
| 02 | [Rust agent](02-rust-agent.md) | Process supervision, metrics, adb, transport, buffer, packaging; PoC measurements |
| 03 | [Rust server](03-rust-server.md) | axum/sqlx/object_store/auth stack, live-state hub, ingestion path; PoC compile checks |
| 04 | [Frontend: Rust vs TS](04-frontend-rust-vs-ts.md) | Leptos, Dioxus, hypermedia and React compared; bundle and update benchmarks |
| 05 | [Protocol and data model](05-protocol-and-data-model.md) | Wire protocol, state machines, evidence lattice, reconciliation, PG 18 DDL, volumes |
| 06 | [CI, security, deployment](06-ci-security-deployment.md) | GitHub Actions identity and OIDC, emulators in CI, threat model, object storage, Compose |
| 07 | [Prior art and build vs buy](07-prior-art-and-build-vs-buy.md) | Maestro Cloud, ReportPortal, Allure, SaaS options; flakiness and fingerprinting methods |
| — | [fixtures/maestro-cli](fixtures/maestro-cli/) | `maestro test` options, copied from the v2.12.1 picocli source |

---

## 1. Is Rust the right call?

| Component | Verdict | Why |
|---|---|---|
| **Agent** | **Strongly yes** | One static 6–7.6 MB binary per target **[M]**, no interpreter on runners, ~0.02% of one core and ~10 MB RSS with a live TLS WebSocket **[M]**. Precise control over signals, process groups and non-blocking pipes, which is exactly what "never alter Maestro's exit code" needs. Python would need PyInstaller-style bundling and would be harder to make crash-proof. |
| **Server** | **Yes**, at modest extra cost | axum 0.8 + sqlx 0.9 + object_store 0.14 covers every requirement **[V]**. Estimated 15–30% more backend effort than TS/Fastify **[I]**, paid back in memory use, a single small image (~35–40 MB) and types shared with the agent. A few ecosystem gaps need small workarounds: a custom tower-sessions Postgres store (~60 lines), TLS feature flags so corporate CAs work, and explicit WebSocket size limits (03 §0). |
| **Shared protocol crate** | **Yes, and the biggest structural win** | `fleet-protocol` (serde + schemars) is shared by agent, server and load generator. JSON Schema and OpenAPI come from the same types. |
| **Dashboard** | **This is where it's contested** | See §5. Report 04 recommends React + TS with types generated from Rust (ts-rs). If all-Rust is a firm requirement, the fallback is server-rendered Rust (askama + Datastar/SSE) with ~1–2k lines of JS for charts and the timeline. A Leptos SPA is possible, but needs ~4–10 extra engineer-weeks, and its accessible component library is not ready. |

---

## 2. Maestro's live signals: the largest correction to the spec

The spec assumes "parsed live console events". The Maestro 2.12.1 source shows that with `--format junit` (or any directory input), **stdout prints no flow-start lines and no step lines.** It prints only one `[Passed]/[Failed] <name> (<dur>)` line after each flow ends, then a summary (01 §0, §3) **[V]**.

What a wrapper *can* see live:

| Signal | Source | Live? |
|---|---|---|
| Planned flow list, shard ↔ device serial | `maestro.log`: `Created execution plan…`, `[shard N] Selected device …` | Yes, seconds after start **[V]** |
| Flow started | `maestro.log` `Running flow`; flow folder created | Yes **[V]** |
| Step running/completed/failed/skipped | `maestro.log` (or stdout only with `--verbose`) | Yes **[V]**; flush-per-line is **[I]**, so Phase 0 must confirm |
| Step failed + screenshot | `screenshots/step-NNN-*.png` appears when the step fails | Yes **[V]** |
| Flow completed + outcome | stdout line; `manifest.json` + `commands.json` written at flow end | Yes, provisional **[V]** |
| Final authoritative results | JUnit, written **once, after all shards finish** | End only **[V]** |
| Skipped / interrupted flows | Never reported locally | **Inferred** only |
| Flow-level retry | Doesn't exist locally (Cloud only); the `retry` command is a step | Not available **[V]** |

Consequences for the design:

- **Agent strategy = three evidence lanes**, merged as provenance claims: (a) a `maestro.log` tailer, which is primary; (b) a stdout parser; (c) a filesystem watcher on the per-flow bundle folders. Then JUnit reconciliation at the end.
- **Canonical invocation.** The agent sets `MAESTRO_CLI_LOG_PATTERN_FILE` to get a machine-friendly log line (epoch-ms, thread, CRLF-escaped message), disables analytics and the update check, and recommends `--flatten-debug-output --debug-output <fresh dir>` plus pinned `--device`. The spec's `--test-output-dir X --debug-output X` puts outputs in two subtrees (01 §14).
- **The artifact layout changed completely in 2.7.0 (July 2026).** Parsers need version profiles and a "degraded" mode that never makes up pass/fail.
- **Exit code `1` means both "tests failed" and "Maestro crashed".** The agent tells them apart by whether a report exists and by the stderr tail. On SIGINT/SIGTERM, Maestro writes **no JUnit at all** **[V]**.
- **Secrets leak:** every `-e` value and every `MAESTRO_*` env var is serialized into `commands.json` **[V]**. Redact before upload, and document that secrets must not be passed through Maestro variables in clear text.
- **Upstream opportunity:** Maestro has a clean `OrchestraListener` interface. A `--events <file>` NDJSON reporter would be a ~200–300-line PR (01 §12) **[V]** and would make the live view far more robust. Worth proposing early; switch to it when it ships.

---

## 3. Agent architecture (Rust)

From 02 §15. The PoC measured the key behaviours.

```
CI shell ──waits on──► P1 SUPERVISOR (std only, no tokio, no network)
                         ├─ tees Maestro stdout/stderr byte-for-byte to CI log  [M: hashes identical]
                         ├─ forwards signals to Maestro's process group
                         ├─ exits with Maestro's code, or re-raises the same signal
                         ├─► C  maestro (JVM) ── dadb ── adb server :5037
                         └─► P2 OBSERVER (tokio): log tailer · fs watcher · adb track-devices ·
                                metrics · state actor · outbox/spool · WSS transport · uploader
```

Rules that matter most:

- **Fail open.** Bad config, credentials or server: Maestro still runs, unmonitored, with a warning. Connecting never blocks starting Maestro.
- P1 → P2 is a **non-blocking pipe**. If P2 stalls, P1 drops the *copy*, never the CI output.
- Spawn Maestro **from the main thread**. `PR_SET_PDEATHSIG` fires when the spawning *thread* exits, and the PoC saw it kill a healthy child, which would turn a pass into exit 137 **[M]**.
- **GitHub cancellation window:** SIGINT, 7.5 s, SIGTERM, 2.5 s, then kill **[D]**. That is shorter than Maestro's shutdown hook. Send `session.interrupted` at the *first* signal.
- **Devices:** speak the adb server protocol directly (`host:track-devices`, push-based). Never start or kill the adb server. Capture `dumpsys` "current activity" only on failure, not every 5 s.
- **Metrics:** targeted per-PID refresh (~0.15 ms) instead of a full scan (~18–21 ms) **[M]**. Read cgroup v2 quota, throttling and OOM files inside containers. macOS thermal comes from `NSProcessInfo.thermalState` only; Apple Silicon CPU temperature needs private APIs, so report it as "unavailable".
- **Buffer:** in-memory priority outbox plus an append-only CRC-framed spool file. No embedded DB.
- **Transport:** tokio-tungstenite + rustls, with explicit HTTP CONNECT proxy and custom-CA support (verified through a proxy **[M]**), and an HTTPS batch-POST fallback for networks that block WebSockets.
- **Distribution:** cargo-zigbuild for Linux musl (x86_64, aarch64). macOS is built natively on a macOS runner (cargo-dist won't cross-compile to macOS). Ship checksums, GitHub attestations and minisign signatures, and serve binaries from the Fleet server itself.

---

## 4. Protocol and data model

From 05. These are design proposals that address 30 gaps found in the spec (05 §14).

- **Four identities, not one "session":** **job** (CI natural key, stable across agent restarts); **agent run** (one `maestro-fleet run` process, a UUIDv7 made by the agent, owns `seq`); **connection** (fenced by an `epoch`); **event** = `(agent_run_id, seq)`. A job can contain several agent runs.
- **Two streams on one socket:** a gap-free **journal** for lifecycle events and evidence (spooled, durably acked, never silently dropped; an explicit `journal.gap` marker if it overflows), and a lossy **telemetry** stream with its own `tseq` that may be coalesced or dropped.
- **The server is authoritative for the resume point** (`welcome.committed_through`). Acks are cumulative and sent after the Postgres commit, and the cursor moves in the same transaction.
- **Encoding:** JSON text frames with a generated JSON Schema. Traffic is small, so binary formats aren't worth losing debuggability.
- **Provenance in the schema (spec §18):** a flow's outcome is *derived* from **claims** ranked `junit_final > artifact_file > live_console > process_exit > server_inferred`. The merge is order-independent. A database `CHECK` forbids inferred sources from claiming `passed`. This turns acceptance criterion 15 into a mechanical guarantee.
- **JUnit matching:** positional per shard (one `<testsuite>` per shard, in shard order), falling back to `file=` and then name. This handles duplicate names and `--shard-all`.
- **Job identity on GitHub:** `job.check_run_id` and the matching OIDC claim (2025) are unique per matrix leg and re-run **[V]**. `GITHUB_JOB` is not.
- **PostgreSQL 18** (18.6; native `uuidv7()`). The app manages native partitions (no TimescaleDB, no pg_partman). Rollups at 1 min and 1 h. Raw 5 s samples are kept 7 days, 1-minute data 90 days, aggregates 1 year. Worst case of 50 workers × 4 devices: ~0.76 GB/day raw telemetry and ~8.7 GB for 90 days of rollups. Step rows and the event journal, not metrics, are the main storage cost.
- **Artifacts:** content-addressed by sha256 **per project** (no cross-tenant dedup). Retention class is decided *after* the outcome is known, so a screenshot is promoted to failure evidence when its flow fails.

---

## 5. Frontend decision (still open)

| Option | Description | Score (04 §5) | When to choose |
|---|---|---|---|
| **E** | React + TS, types from Rust via **ts-rs**, SPA embedded in the axum binary | **115/125**, recommended | IT or non-Rust developers will maintain it; fastest path to a polished, accessible UI |
| **D** | Server-rendered Rust (askama + **Datastar 1.0** over SSE) plus 3 small JS islands (uPlot, canvas timeline, log viewer) | 90/125, all-Rust fallback | "All-Rust" means all logic and state live in Rust; backend-leaning team |
| **A+F** | **Leptos 0.8 CSR** + uPlot interop | lower | The team is Rust-fluent, enjoys UI work, and accepts a 0.x framework with a bus factor of about 2 |

Measured: a Leptos dashboard skeleton came to 127 KB gzip / 163 ms start, versus 82 KB / 183 ms for the same skeleton in React **[M]**. Leptos only beat React under live updates when state was split into fine-grained signals, and a keyed list showed stale rows after a re-snapshot **[M]**. That re-snapshot is exactly the pattern spec §6 relies on. No maintained, accessible Leptos component library is released for 0.8, and nobody offers a Gantt/timeline component **[V]**.

**Our read:** if "the whole thing in Rust" means *Rust owns the system's correctness*, choose **E** or **D**. The agent, server, protocol, state machines and reconciliation are all Rust either way. Choose Leptos only if writing the UI itself in Rust is a goal in its own right. → Decision recorded as *proposed* in [ADR-0004](../docs/decisions/0004-frontend.md).

---

## 6. Security, CI and deployment highlights

From 06.

- **Agent auth:** exchange the GitHub OIDC token for a short-lived, write-only Fleet token. Authorize on the immutable `repository_id`/`repository_owner_id` claims, **never on `sub`**: its format changed for repos created or renamed after 2026-07-15 **[V]**. Take run, attempt and job identity from the verified claims; environment variables are display-only.
- **Fork PRs** get no OIDC token and no secrets. The default is **offline mode**: Maestro runs and its exit code is kept, a local bundle is written, nothing is published. A trusted `workflow_run` relay can be an opt-in, labelled `untrusted`. `pull_request_target` is explicitly unsupported.
- **User HTML** (Maestro HTML reports) is served only from a **separate usercontent domain** with `CSP: sandbox` + `nosniff`, or offered as a download. IT has to provide that DNS name and certificate.
- **Object storage:** **MinIO community edition is no longer a viable default.** Its console was removed in 2025, binaries and images stopped in October 2025, and the repo was archived on 2026-04-25 **[V]**. Default to **Garage** (Rust, AGPLv3), with **SeaweedFS** (Apache-2.0) if AGPL is a problem, plus a built-in **filesystem backend**. Fleet relays uploads by default.
- **WebSocket idle timeouts:** nginx, ALB and Traefik default to 60 s and Cloudflare to ~100 s. Ping every 15–25 s. Caddy needs `stream_close_delay` set, or WebSockets drop on config reload.
- **Emulators in CI:** GitHub-hosted macOS arm64 runners **cannot** run the Android emulator (no nested virtualization) **[V]**. Use Linux x64 + KVM, or self-hosted Apple Silicon with arm64 images.
- **SSO:** Entra ID drops the `groups` claim above 200 groups (overage). Prefer app roles, and deny when group claims are missing.
- **Postgres 18 Docker image:** mount the volume at `/var/lib/postgresql`, not `/var/lib/postgresql/data`.

---

## 7. Build vs buy

From 07. **Verdict: a focused build ("A′").**

- **Nothing on the market** gives live Maestro progress from company-run CI emulators correlated with host and device health. Every live Maestro view comes from a vendor running its own devices (Maestro Cloud at $250 per device per month, BrowserStack and others) **[V]**.
- **History, trends and flaky ranking are commodity** (ReportPortal, Allure 3, SaaS). Keep them thin in Fleet, as SQL over its own tables. Don't build ML triage, a time-series DB, quality gates or test management.
- **Fallback if capacity is short:** build the agent and live view, then push final results to ReportPortal or Allure 3. ReportPortal is about 12 containers (OpenSearch, RabbitMQ) and announced in April 2026 that SSO/LDAP is moving to paid. A 1–2 day Phase 0 add-on can measure what that path loses.
- **Flakiness v1:** *confirmed* = pass and fail on the same commit under the same conditions; *suspected* = flip rate ≥ 0.2 over the last 20 default-branch runs. Infrastructure failures and failures during host anomalies are excluded. A flaky label never hides a failure: in a Chromium study, flaky tests still caught more than a third of real regressions **[V]**.
- **Failure fingerprint:** Maestro exception type + failing step identity + normalized message (selector text kept), versioned, with user override rules.
- **Interoperability:** use OpenTelemetry `cicd.*`/`test.*` attribute names in the schema (the CI/CD conventions are release candidates) and add an optional OTLP export from the agent.

---

## 8. Proposed spec changes (summary)

| Spec § | Change |
|---|---|
| §2A, §4 | Rust agent: supervisor/observer split, fail-open, signal re-raise, writes nothing to stdout |
| §3, §7 | Identity: pipeline `(provider, server_url, repository_id, run_id, run_attempt)`; job `check_run_id`; agent run ≠ job; device *session* ≠ device *profile*; `tested_sha` vs `head_sha` |
| §3 Attempt | Maestro has no local flow retries. "Attempt" is derived across CI re-runs and invocations, not from Maestro output |
| §4 Progress | Live sources = `maestro.log` + flow bundle folders + stdout; console-only scraping is insufficient. Recommended invocation flags. Version-profiled parsers |
| §4 Devices | adb `track-devices`; no 5 s `dumpsys`; cgroup-scoped metrics; macOS temperature always unavailable |
| §4 Statuses | No `skipped` from Maestro; skipped/interrupted/unknown are inferred and labelled as such |
| §5 | Journal and telemetry streams; server-authoritative resume; cumulative post-commit acks; explicit gap markers; ping ≤ 25 s; CONNECT proxy + custom CA; HTTPS fallback |
| §5 Event list | Add plan, report parsed/absent, process exit, CI signal, artifact uploaded/rejected, device lost, journal gap, parser drift. Merge `flow.completed`/`flow.failed` into one outcome claim |
| §6 | axum/sqlx stack; versions persisted in PG; liveness timers restart at `max(last_seen, boot)` + grace; LISTEN/NOTIFY (not Redis) for multi-instance; `migrate`, `check-config`, `healthcheck` subcommands |
| §6 Job states | Separate *lifecycle* from *outcome*; "Final CI result" only when a provider integration fills it |
| §7 | PG 18; native partitions; retention 7 d raw / 90 d 1-min / 1 y aggregates; per-project content-addressed blobs; retention promoted at finalization |
| §8 | Frontend per ADR-0004 (open) |
| §10 | OIDC exchange on immutable claims; trust levels; fork PRs offline; redaction of `-e`/`MAESTRO_*`/debug JSON; separate usercontent origin |
| §11 | Single `fleet` image; Caddy reference proxy; Garage default (MinIO CE not recommended); filesystem backend; PG 18 volume path |
| §12 | Runner guidance: Linux x64 + KVM or self-hosted Apple Silicon; not GitHub-hosted macOS arm64 |
| §14 | Phase 0 grows: the `maestro.log` liveness tests and the ReportPortal/Allure comparison below |

---

## 9. Phase 0: what must be verified on a real emulator

Everything above that rests on reading source code still has to be confirmed in a real run. The full lists are in 01 §15 and 05 §15. The gating items:

1. `maestro.log` is flushed per line, so a tailer sees `RUNNING` before `COMPLETED` in real time. **If this fails, live step progress falls back to per-flow granularity.**
2. `MAESTRO_CLI_LOG_PATTERN_FILE` with `%d{UNIX_MILLIS}`, `%t` and `%enc{%msg}{CRLF}` works with the bundled log4j.
3. Filesystem event trace for write timing: flow folder at start, failure screenshot at step failure, `commands.json` then `manifest.json` at flow end, report at suite end.
4. JUnit samples: single run, `--shard-split 2`, `--shard-all 2`, two emulators of the same AVD (is `device=` ambiguous?).
5. Exit codes and on-disk leftovers for: pass, fail, no device, bad flag, device killed mid-run, SIGINT, SIGTERM, SIGKILL.
6. The agent querying adb concurrently (getprop, meminfo, logcat) does not cause Maestro flakiness.
7. Overhead with and without the agent, on a real suite.
8. Optional: push the same fixtures through ReportPortal and Allure 3 to inform the build-vs-buy fallback.

Store all captured output under `research/fixtures/` as regression fixtures; parser tests should run against them (insta snapshots).

---

## 10. Decisions for you

1. **Frontend:** E (React + TS + ts-rs), D (server-rendered Rust + Datastar), or Leptos? See [ADR-0004](../docs/decisions/0004-frontend.md).
2. **Object storage licence:** is AGPL acceptable to your company (Garage)? If not, use SeaweedFS.
3. **Fork PR policy:** offline only, or also the `workflow_run` relay with an `untrusted` label?
4. **Upstream:** propose a `--events` NDJSON reporter to Maestro?
5. **Scope:** full A′ build, or the hybrid fallback (agent + live view + ReportPortal/Allure for history)?

## 11. Known limits of this research

- Nothing here ran against a real emulator or a real Maestro process (Maestro was read from source, not executed).
- The PoCs ran on one 4-vCPU x86_64 Linux VM. Nothing ran on macOS or arm64.
- Docker and a local Postgres weren't available, so database throughput and S3 round-trips are estimates.
- The GitHub API was blocked during research, so the survey of community Maestro tools may have missed small projects.
