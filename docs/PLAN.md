# Maestro Fleet: Implementation Plan (all-Rust)

**Version:** 1.0 · **Date:** 2026-11-03 · **Owner:** repository owner (personal GitHub account)
**Inputs:** [spec v1.0](spec-v1.0.md), [research synthesis](../research/00-synthesis.md), research reports 01–07, ADRs in [decisions/](decisions/README.md)

This document is the single source of truth for building the project. It is written so that a coding agent can execute it top to bottom. When this plan disagrees with spec v1.0, **this plan wins**: it includes the research findings and the owner's decisions.

---

## 0. Instructions for the executing agent

1. **Read first:** this file → `research/00-synthesis.md` → the ADRs. Read the individual research reports when working on that area (each section below names its report).
2. **Do Milestone M0 (§12) exactly.** It creates the repository, the licensing files, the workspace skeleton and CI. Do not write product logic in M0.
3. **Work in milestone order.** Each milestone has exit criteria; don't start the next until they pass or the owner waives them.
4. **The invariants in §3 are non-negotiable.** If a task seems to need breaking one, stop and ask the owner.
5. **Versions in §5 were current on 2026-11-03.** Use the latest *patch* release of each when you start. Do not jump minor or major versions (for example Leptos 0.9, axum 0.9) without a separate ADR.
6. **Never invent Maestro output.** Parsers and tests must be built from captured fixtures (Phase 0, M1) or from quoted source. Mark anything else `// UNVERIFIED`.
7. **Commits:** Conventional Commits (`feat(agent): …`, `fix(server): …`). One logical change per PR. Every PR must pass `cargo xtask ci`.
8. **Fetch license texts verbatim from their official sources** (§2). Never type or paraphrase legal text from memory.

---

## 1. Decisions already made

| # | Decision | Detail / ADR |
|---|---|---|
| D1 | **Everything in Rust**: agent, server, protocol, **and the dashboard** | ADR-0001, ADR-0004 (decided: Leptos CSR) |
| D2 | ~~Source-available, not open source~~ **Open source under AGPL-3.0-only**, contributions under a CLA, commercial license on request, donations welcome | ADR-0012 (supersedes ADR-0010) |
| D3 | Hosted on the owner's **personal** GitHub account; the owner's employer uses it under the same license | §2.5 (employer IP check is a precondition) |
| D4 | Focused build: live view + correlation + evidence; history and analytics kept thin | ADR-0009 |
| D5 | Postgres 18 only; S3-compatible storage via `object_store`; Garage as the reference store; filesystem backend built in | ADR-0006, ADR-0007 |
| D6 | GitHub Actions OIDC for publishers; fork PRs run offline | ADR-0008 |
| D7 | No Node.js, npm or hand-written JavaScript anywhere in the product or its build | §3 invariant I9 |
| D8 | **Fleet Planner**: static, duration-aware sharding across CI jobs (LPT on Fleet history); no dynamic work queue in v1 | ADR-0013 |

**Defaults for the decisions still open** (the owner may override them):

| # | Question | Default |
|---|---|---|
| O1 | Project name. "Maestro" is mobile.dev's product name; a name that starts with it can look official | **Decided: Fleet** ([ADR-0011](decisions/0011-name-and-runner-scope.md)). Agent binary `fleet-agent`, repository `moyaver/fleet`; Maestro first, runner-agnostic core. Older names in this plan, the spec and the research map as listed in the ADR. |
| O2 | Fork-PR relay (`workflow_run` publishing with an `untrusted` label) | Not in v1. Offline mode only. |
| O3 | Upstream `--events` NDJSON reporter PR to Maestro | Propose it after Phase 0 (M1). The agent keeps working without it. |

---

## 2. Licensing, contributions and donations

> **Superseded (2026-11-04):** Fleet is now open source under AGPL-3.0-only ([ADR-0012](decisions/0012-agpl-license.md)). This section is kept as history; the CLA, donations and dependency-license rules still apply.

### 2.1 What the owner asked for, and what that means legally

"Open source, but nobody may copy it, only contribute and donate" can't be done under an *open source* license. Every OSI-approved license must allow redistribution and derived works. The correct term is **source-available**. Say "source-available" in the README, never "open source".

### 2.2 License: PolyForm Internal Use License 1.0.0

This is a standard, lawyer-drafted license that matches the request:

- **Use:** "Use of the software for the internal business operations of you and your company is use for a permitted purpose." So any company, including the owner's employer, can run it internally for free.
- **Changes:** may be made "for any permitted purpose", so a company can patch it for its own internal use.
- **Distribution:** "you may not distribute the software." No re-publishing, no reselling, no rebranded copies, no SaaS-style redistribution of the code.
- **Patents:** it includes a patent license from the licensor.

Implementation:

1. Fetch the exact text from the official source, polyformproject.org: PolyForm Internal Use 1.0.0. Save it as `LICENSE.md` without changing a word, then fill in only the "Required Notice" line, e.g. `Required Notice: Copyright (c) 2026 <Owner Name> (https://github.com/<owner>)`.
2. **Cargo:** this license isn't in SPDX, so every crate uses `license-file = "../../LICENSE.md"` (adjusted per crate) instead of `license = …`. Also set `publish = false`: crates.io requires redistribution rights, and these crates are never published there.
3. Add an SPDX-style header comment to every source file: `// SPDX-License-Identifier: AGPL-3.0-only`. Enforce it in `cargo xtask lint`.
4. Add `LICENSING.md`, a plain-language FAQ, which must say:
   - It is source-available, not open source.
   - Internal use by any organization is free.
   - Redistribution, public forks of modified versions, and offering it to third parties are not permitted.
   - Forking on GitHub *to open a pull request* is expected and fine. GitHub's Terms of Service already let users fork public repositories on GitHub. That gives no rights outside GitHub and no rights to distribute.
   - For other uses, e.g. a commercial or redistribution license, contact the owner.
   - It is not legal advice; the license text governs.

**Alternative**, only if the owner wants *only their own employer* to be able to use it: PolyForm Strict 1.0.0 (noncommercial use only, no changes, no distribution) plus a separate written license grant to the employer. This is more restrictive and harder to manage. **The default is Internal Use.**

### 2.3 Contributions: a CLA is required

Contributors keep copyright in their code unless they grant rights. To keep the project relicensable (for example, a commercial license later) and to stop contributors' code coming with conflicting terms:

- **`CLA.md`** is an Individual Contributor License Agreement. It grants the owner a perpetual, worldwide, royalty-free, irrevocable copyright and patent license to the contribution, **including the right to sublicense and relicense**. It is not a copyright assignment.
  - Base it on the Apache Software Foundation Individual CLA. Fetch that text verbatim, then change only the names and the project. Mark the file as adapted.
- **`CLA-CORPORATE.md`** is the same, adapted from the ASF Corporate CLA. **It is required for contributions made on company time, including by the owner's coworkers**, because their employer owns that work.
- **Automation:** add `.github/workflows/cla.yml` using the *CLA Assistant Lite* GitHub Action (`contributor-assistant/github-action`). It stores signatures in the repo on a `cla-signatures` branch, so no third-party service is needed.
  - Before adopting it, verify the action's maintenance status and pin it by commit SHA.
  - If it is unmaintained, write a small workflow that checks for a signature comment.
- **`CONTRIBUTING.md`** covers: the CLA requirement, the dev setup (`cargo xtask setup`), the PR checklist, the ADR process, commit conventions, and "no AI-generated legal text".
- **`CODE_OF_CONDUCT.md`:** Contributor Covenant 2.1, fetched verbatim.
- **`SECURITY.md`:** private vulnerability reporting through GitHub Security Advisories. No public issues for vulnerabilities.

### 2.4 Donations

- **`.github/FUNDING.yml`** lists `github: [<owner>]` (GitHub Sponsors). Optional: `ko_fi`, `buy_me_a_coffee`, `custom`.
- Mention sponsorship in the README and `LICENSING.md`. Donations buy no license rights; say so explicitly.
- **Do not plan on Open Collective's "Open Source Collective" fiscal host.** Its eligibility guidance expects an open source license. Confirm with them before applying.
- Optional: offer paid support or custom licensing on request. That's the owner's call.

### 2.5 Precondition: employer IP check (owner action, before M0 code)

The owner works at the company that will use this, and it solves that company's problem. Many employment agreements assign to the employer any IP that is created on company time or equipment, or that relates to the employer's business.

**Before writing product code, the owner should:**
1. Read their employment agreement's IP or invention-assignment clause.
2. Get **written** confirmation from the employer (manager plus legal/HR) that the project is the owner's personal IP and that the company will use it under the project license.
3. Build it on personal time and equipment, and on the personal GitHub account.
4. Have coworker contributions go through the Corporate CLA (§2.3).

This plan is not legal advice. A short review by a lawyer of the license choice, the CLA and the employer letter is recommended.

### 2.6 Third-party dependency licenses

Licensing the project's own code under PolyForm is compatible with permissive dependencies. Shipping binaries still requires including their notices.

- **`deny.toml` (cargo-deny):**
  - Allow MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unicode-DFS-2016, CC0-1.0, MPL-2.0 (file-level copyleft; acceptable, unmodified).
  - **Deny** GPL-*, AGPL-*, LGPL-*, SSPL and other copyleft or non-commercial licenses in anything linked into a binary.
  - Also deny advisories, yanked crates and unknown registries.
- **`cargo about`** generates `THIRD_PARTY_LICENSES.html` for every release artifact and container image.
- **Garage (AGPLv3) is a separate service** in the reference Compose file, reached over the S3 API. Fleet does not link it. Document that AGPL obligations apply to Garage only. SeaweedFS (Apache-2.0) is the documented alternative.

---

## 3. Invariants (non-negotiable)

| # | Invariant | Enforced by |
|---|---|---|
| I1 | **The agent never changes Maestro's exit status or CI output.** Pass-through is byte-for-byte; a signal death is re-raised | Supervisor/observer split (ADR-0002); `agent-supervisor` integration tests with a fake Maestro |
| I2 | **Fail open:** Fleet misconfiguration or outage never prevents or fails a Maestro run (except in `check`/`selftest`) | Tests run with no server, a bad token and a black-holed network |
| I3 | **Nothing is ever falsely successful:** only direct-observation provenance may claim `passed` | `fleet-core` claim lattice + DB `CHECK` constraint + property tests |
| I4 | **Every displayed result carries a provenance** (observed / provisional / final / derived / inferred / missing / stale) | The type system: every DTO outcome is a `Claimed<T>`; UI badges |
| I5 | **The journal is never silently lossy:** lifecycle and evidence are gap-free or carry an explicit `journal.gap` | Protocol tests; outbox property tests |
| I6 | **No secret leaves the runner unredacted:** `-e` values, `MAESTRO_*` values, secret-named env vars, known patterns | Redaction in agent and server; fixtures with planted secrets |
| I7 | **The browser never talks to runners, and user-uploaded HTML never runs on the app origin** | Separate usercontent origin + `CSP: sandbox` |
| I8 | **No provider-specific code** (AWS, GCP, Azure, Cloudflare, Kubernetes) in core crates | Crate dependency rules in `cargo xtask lint` |
| I9 | **No Node.js, npm or hand-written JS.** Only Rust, SQL, CSS, HTML templates, YAML (CI) and shell snippets in docs | `cargo xtask lint` fails on `package.json`/`*.js`/`*.ts` outside `target/` and `dist/` |
| I10 | **The agent stays light:** < 1% host CPU on average, bounded memory, no full process scans in steady state | `agent.self_metrics` + overhead benchmark (M2 exit) |

---

## 4. Repository layout

```
maestro-fleet/
├─ Cargo.toml                 # [workspace] members, [workspace.package], [workspace.dependencies], lints
├─ Cargo.lock                 # committed
├─ rust-toolchain.toml        # channel = latest stable (1.99 at M0), components rustfmt, clippy; targets wasm32-unknown-unknown,
│                             # x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, aarch64-apple-darwin
├─ .cargo/config.toml         # alias xtask = "run -p xtask --"
├─ deny.toml  about.toml  about.hbs  rustfmt.toml  clippy.toml  .editorconfig  .gitignore
├─ LICENSE.md  LICENSING.md  CLA.md  CLA-CORPORATE.md  CONTRIBUTING.md  CODE_OF_CONDUCT.md  SECURITY.md
├─ README.md  CHANGELOG.md
├─ .github/
│  ├─ FUNDING.yml  CODEOWNERS  pull_request_template.md  ISSUE_TEMPLATE/{bug,feature,maestro-version}.yml
│  └─ workflows/ ci.yml  cla.yml  release.yml  image.yml  nightly-emulator.yml  audit.yml
├─ crates/
│  ├─ fleet-protocol/         # wire types, REST DTOs, IDs, JSON Schema (schemars) — no tokio, wasm-safe
│  ├─ fleet-core/             # pure domain: state machines, claim lattice, reconciliation, fingerprinting,
│  │                          # flakiness, redaction, policy (RBAC), LTTB downsampling — no I/O
│  ├─ fleet-maestro/          # Maestro knowledge: version profiles, maestro.log / stdout / bundle / JUnit
│  │                          # parsers, invocation builder — no I/O beyond &str/&[u8] input
│  ├─ fleet-agent/            # `maestro-fleet` binary: supervisor + observer
│  ├─ fleet-store/            # sqlx repositories, ingest transaction, partitions, rollups, migrations
│  ├─ fleet-blob/             # BlobStore trait: S3 (object_store) + LocalFs + signed upload tokens
│  ├─ fleet-server/           # `fleet` binary: serve | migrate | check-config | healthcheck | create-admin
│  ├─ fleet-ui/               # Leptos component kit (accessible primitives, charts, timeline, log viewer)
│  ├─ fleet-web/              # Leptos CSR app (screens A–G), built by Trunk → dist/
│  ├─ fleet-web-assets/       # embeds fleet-web/dist into the server (rust-embed), keeps rebuilds fast
│  ├─ fleet-loadgen/          # protocol load generator (simulated agents)
│  └─ fleet-e2e/              # browser E2E tests (thirtyfour/WebDriver) + full-stack scenarios
├─ xtask/                     # cargo xtask: setup ci fmt lint test web sqlx fixtures dist licenses compose
├─ migrations/                # 0001_init.sql … (forward-only, expand/contract)
├─ .sqlx/                     # sqlx offline query metadata (committed)
├─ deploy/
│  ├─ compose/ docker-compose.yml  Caddyfile  garage.toml  .env.example
│  └─ docker/ Dockerfile (cargo-chef → distroless/cc)
├─ examples/
│  ├─ github-actions/maestro-fleet.yml     # wrapped Maestro job with OIDC
│  └─ flows/                               # sample flows for the nightly emulator job
├─ fixtures/maestro/<version>/<scenario>/  # captured stdout, stderr, maestro.log, bundles, JUnit, exit code, fs trace
├─ docs/                      # mdBook: user guide, operator guide, runner guide, protocol reference
│  ├─ book.toml  src/SUMMARY.md …
│  ├─ PLAN.md  spec-v1.0.md  decisions/
└─ research/                  # research reports 00–07 (kept as-is for reference)
```

### 4.1 Naming in one place

The product name, binary names (`maestro-fleet`, `fleet`), env-var prefix (`FLEET_`), WS subprotocol (`fleet.v1+json`) and OIDC audience default are defined as constants in `fleet-protocol::brand` and in `docs/book.toml`. A rename then touches only these places plus the crate directory names.

### 4.2 Dependency rules (checked by `cargo xtask lint`)

- `fleet-protocol` → only serde, serde_json, uuid, time, schemars (feature) and utoipa (optional `openapi` feature, enabled only by fleet-server for OpenAPI). No async runtime, must build for wasm32.
- `fleet-core`, `fleet-maestro` → `fleet-protocol` + pure crates. No tokio, sqlx, reqwest or filesystem I/O.
- `fleet-agent` → `fleet-protocol`, `fleet-core`, `fleet-maestro`. **Never** `fleet-store`/`fleet-server`.
- `fleet-web`, `fleet-ui` → `fleet-protocol`, `fleet-core` (wasm-safe subset). No tokio.
- `fleet-server` → everything except `fleet-agent`, `fleet-web`.

---

## 5. Technology stack (pinned as of 2026-11-03)

### 5.1 Workspace-wide
| Purpose | Choice |
|---|---|
| Toolchain | Latest stable Rust (1.99 at M0), edition 2024 |
| Serialization | `serde` 1, `serde_json` 1 |
| IDs | `uuid` 1 (v7, serde) |
| Time | `time` 0.3 (or `jiff`; pick one in M0, ADR) |
| Errors | `thiserror` 2 (libraries), `anyhow` 1 (binaries only) |
| Logging | `tracing` 0.1, `tracing-subscriber` 0.3 (JSON) |
| Tests | `cargo-nextest` 0.9, `insta` 1.49, `proptest` 1.12, `assert_cmd` 2.2, `wiremock` 0.6 |
| Supply chain | `cargo-deny`, `cargo-about`, `cargo-audit` (via deny) |
| Docs | `mdbook` |

### 5.2 Agent (research 02 §16)
| Purpose | Crate |
|---|---|
| Supervisor signals | `signal-hook` 0.4, `nix` 0.31, `libc` 0.2 (std threads, no tokio) |
| Observer runtime | `tokio` 1.54 (`process`, `signal`, `net`, `time`, `fs`, `io-util`), `tokio-util` 0.7, `bytes` 1 |
| Metrics | `sysinfo` 0.40 (`default-features=false`, `system`; targeted per-PID refresh), hand-parsed `/proc/pressure/*` and cgroup v2 files |
| macOS thermal | `objc2-foundation` (`NSProcessInfo::thermalState`); CPU temperature is always `unavailable` |
| File watching | `notify` 8.2 + `notify-debouncer-full` 0.7, plus a periodic 15 s reconcile scan |
| adb | Own minimal adb **server-protocol** client (`host:track-devices`, `host-serial:<s>:…`, `shell:` with timeouts). Never start, restart or kill the adb server. |
| WebSocket | `tokio-tungstenite` 0.30 over `hyper-util` 0.1 proxy `Tunnel` (CONNECT) + `tokio-rustls` |
| TLS trust | `rustls-platform-verifier` 0.7, `webpki-roots` 1 fallback, `FLEET_CA_FILE` custom bundle |
| Uploads | `reqwest` 0.13 (rustls, stream), `sha2` 0.11, `infer` 0.22 |
| Backoff | `backon` 1.6 |
| Spool | Own append-only segment file (length + `crc32fast` frames) |
| Parsing | `quick-xml` 0.42 (serialize), `regex` 1, `strip-ansi-escapes` 0.2, `semver` 1 |
| CLI/config | `clap` 4.6 (derive, env), `toml` 1, `secrecy` 0.10 |
| Packaging | `cargo-zigbuild` 0.24 (Linux musl x86_64 and aarch64), native macOS arm64 build, `cargo-dist` 0.34 releases |

### 5.3 Server (research 03 §2)
| Purpose | Crate |
|---|---|
| HTTP/WS | `axum` 0.8.9, `tower` 0.5, `tower-http` 0.7 (trace, compression, limits, CORS off, set-header) |
| DB | `sqlx` 0.9 (postgres, runtime-tokio, tls-rustls with platform/custom CA, uuid, time, json, migrate) |
| Object storage | `object_store` 0.14 (aws feature, path-style, custom endpoint) + LocalFileSystem |
| Browser SSO | **Own** code flow (auth code + PKCE + nonce) on `reqwest` and the CI-OIDC RS256 verifier, and **own** cookie sessions in Postgres. `openidconnect` 4.1 needs a second `reqwest` (0.12) and pulls the `rsa` crate the verifier avoids; `tower-sessions` has no matching sqlx 0.9 store (research 03 §8.1). |
| Agent auth | `jsonwebtoken` 11.1 (choose a crypto backend feature) + own JWKS cache (refetch on unknown `kid`) |
| Validation | `garde` 0.23, `nutype` 0.8 |
| OpenAPI | `utoipa` 6 + `utoipa-axum` 0.3 + `utoipa-scalar` 0.4 |
| WS schema | `schemars` 1.2 (JSON Schema 2020-12 generated into `docs/src/protocol/`) |
| Config | `figment` (TOML + env `FLEET_*` + `*_FILE` secrets) |
| Ops | `metrics` + `metrics-exporter-prometheus`, `tower_governor`, optional OTLP (`opentelemetry` feature) |
| Assets | `rust-embed` 8 (or `memory-serve` 2) in `fleet-web-assets` |
| Tests | `#[sqlx::test]`, `testcontainers` (Postgres 18, Garage), `axum-test` |
| Image | `cargo-chef` + `gcr.io/distroless/cc-debian12`, multi-arch on native runners |

### 5.4 Dashboard: Leptos CSR, all Rust (research 04 §9.5, adapted to "no JS")
| Purpose | Choice |
|---|---|
| Framework | `leptos` **=0.8.23** (`csr`), `leptos_router` =0.8.17. **No SSR or hydration**; the 0.9 upgrade gets its own ADR after 0.9.0 final. |
| Build | **Trunk 0.22.1**; release profile `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`; `wasm-opt -Oz` |
| Browser APIs | `wasm-bindgen` =0.2.130, `web-sys` 0.3, `js-sys`, `gloo-net` 0.7 (REST), `leptos-use` 0.20 (WebSocket, observers, intervals) |
| Styling | **Tailwind CSS 4 standalone CLI** (a single native binary, no Node), downloaded and checksum-verified by `cargo xtask setup`, run by Trunk's tailwind asset. Design tokens are CSS custom properties with light and dark themes. |
| Components | **Own kit in `fleet-ui`** (no third-party Leptos UI library; none is mature and accessible on 0.8). Built on native elements first: `<dialog>`, the Popover API, `<details>`, `<select>`, `<input type=datetime-local>`. Follow the WAI-ARIA Authoring Practices for anything custom. |
| Charts | **Native Rust**: SVG line/area charts with LTTB downsampling (`fleet-core::downsample`), canvas (`web-sys` `CanvasRenderingContext2d`) above ~5k points. No uPlot or ECharts. |
| Timeline (Gantt) | Native canvas swimlanes (devices × time) with hit-testing, zoom and pan, **plus an equivalent accessible table view** |
| Tables | `fleet-ui::Table` (sorting, filtering, column visibility) + `fleet-ui::VirtualList` (fixed row height windowing) |
| Log viewer | Virtualized; ANSI parsed with `vte` into styled spans; **text nodes only, never `inner_html`** |
| E2E | `fleet-e2e` with `thirtyfour` (WebDriver) + chromedriver. Optional accessibility checks inject vendored `axe-core` **in tests only** (never shipped; exempted from I9 by path). |

**Known Leptos pitfalls to design around** (measured in research 04 §3):
- Use **fine-grained per-entity signals**: a store of `RwSignal<Row>` keyed by entity ID, not one big `Vec` signal. The naive version was slower than React under live updates.
- **After a re-snapshot, change the list's key generation** (`(snapshot_epoch, id)`). A keyed list otherwise kept stale rows with no error.
- Throttle UI application of live deltas to animation frames (`request_animation_frame`), coalescing deltas per entity.

### 5.5 Infrastructure
| Component | Choice |
|---|---|
| Database | PostgreSQL **18** (18.6+); volume at `/var/lib/postgresql` |
| Object storage | Garage (reference) / SeaweedFS / any S3 / built-in filesystem |
| Reverse proxy | Caddy (reference). Set `stream_close_delay`, or WebSockets drop on reload |
| Usercontent | Separate hostname, e.g. `fleet-usercontent.example`, for Maestro HTML reports |

---

## 6. Agent design: `maestro-fleet` (research 01, 02; ADR-0002, ADR-0003)

### 6.1 Commands
| Command | Behaviour |
|---|---|
| `run [opts] -- maestro test …` | Supervise and observe. Exit code = Maestro's. |
| `check` | Config, server reachability (TLS/CA/proxy), auth (OIDC available?), Maestro on PATH and its version, adb server reachable, KVM/HVF acceleration, output dirs writable, clock skew. Fails hard on problems. |
| `selftest` | `check` plus a synthetic run against a fake Maestro, end-to-end to the server |
| `inspect <dir>` | Offline: parse a saved run (stdout capture, `maestro.log`, bundles, JUnit) and print the reconciled result as a table or JSON. Uses the same code path as live. |
| `flush <spool>` | Replay a spool from a crashed run (`if: always()` post-step) |
| `version` | Agent version, protocol versions, supported Maestro version range |

### 6.2 Process model
```
CI shell ──waits on──► P1 supervisor (std only) ──spawn──► C: maestro (own pgid; spawned from main thread)
                         │ tee stdout/stderr → fd1/fd2 (byte-exact), copy → nonblocking pipe → P2
                         │ signals → killpg(C), notify P2, grace timer → SIGKILL
                         │ exit(code) | re-raise(signal)
                         └─spawn──► P2 observer (tokio): ingest · log tailer · fs watcher · device mgr ·
                                     metrics · STATE ACTOR · outbox/spool · transport · uploader
```
- P2 failing to start or crashing → P1 logs one warning line to stderr and carries on.
- P1 waits for P2's finalization at most `finalize_timeout` (default 60 s; configurable), then exits regardless.
- On the first SIGINT/SIGTERM, P2 sends `run.signal_received` with a priority flush. GitHub allows only 7.5 s + 2.5 s.

### 6.3 Invocation handling (`fleet-maestro::invocation`)
- **Parse** the user's `maestro …` argv to learn: format and output path, debug/test output dirs, shard flags, devices, flow paths and config.
- **Inject** env vars (without overriding values the user set):
  - `MAESTRO_CLI_NO_ANALYTICS=1`
  - `MAESTRO_DISABLE_UPDATE_CHECK=true`
  - `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true`
  - `MAESTRO_CLI_LOG_PATTERN_FILE` set to the machine pattern `MFLT1|%d{UNIX_MILLIS}|%t|%level|%logger|%enc{%msg}{CRLF}%n` (validated in Phase 0).
- **Rewrite mode** (`--fleet-rewrite=auto|off`, default `auto`):
  - If neither `--debug-output` nor `--test-output-dir` is given, add `--flatten-debug-output --debug-output $RUN/artifacts`.
  - If no `--format` is given, add `--format junit --output $RUN/report.xml` only when that can't change Maestro's execution mode (`--format` selects its multi-flow path, which rejects `-c`): several flows, a directory, or `--shard-all N` with N > 1, and neither `-c` nor `--continuous`. A single flow, a continuous run, or a path that can't be classified gets no report, and the reason is logged.
  - The report is one normalization over (`--format`, `--output`, injection decision), and Fleet never adds an option the user gave (picocli rejects a repeated `--output` or `--format` with exit 2): `--format junit` alone reads `--output` or Maestro's default `report.xml` in the working directory; `--output X` without `--format` gets only `--format junit` when injection is allowed (and reads `X`), else nothing; another `--format` means no JUnit report by choice. The resulting `EvidenceSources` (report path or reason, debug output, machine log) is sent in `run.started` and stored in `agent_runs.evidence`.
  - Never remove user flags. Log every injected flag.
- **Warn** when several devices are connected but no `--device` is given. Maestro would silently pick the first.

### 6.4 Evidence lanes → claims
| Lane | Source | Produces |
|---|---|---|
| A | `maestro.log` tailer (primary) | plan, shard ↔ device serial, flow started, step running/completed/failed/skipped/warned, command-retry attempts |
| B | stdout parser | flow completed + outcome + duration, suite summary |
| C | fs watcher (bundle folders) | flow folder (start), failure screenshot (step failed), `commands.json` + `manifest.json` (flow end, exact step timings) |
| D | process | exit status, signal, rusage, stderr tail |
| E | final JUnit (reconciliation) | `junit_final` claims |

Every parser is version-profiled (`≥2.7` bundle layout; `<2.7` best-effort). Unknown lines increment `parser.drift` and are never turned into outcomes.

**Exit-code classification:**
- `0` → suite passed.
- `1` + report → test failures.
- `1` + no report → Maestro or infra error.
- `2` → usage error.
- Signal → interrupted.

### 6.5 Metrics (every 5 s, configurable)
- **Host:** CPU, logical cores, memory total/available, swap, load, uptime, PSI. **cgroup** quota, throttling, memory max/current, OOM events (labelled `scope`). Thermal state plus its source.
- **Processes:** Maestro JVM and each emulator `qemu-system-*`, mapped to a serial via the console port in its cmdline. Targeted per-PID refresh only; a full scan only at start and on device change.
- **Devices:** serial, AVD name, API level, boot completed, adb state, assigned shard. Guest memory and CPU are optional, at ≥ 30 s intervals, labelled `guest`. Current activity only on failure.
- **Agent self:** supervisor plus observer (I10). CPU is the sum of both processes' CPU-time deltas since the last sample (the observer from `getrusage`, the supervisor from a targeted refresh of the PID it passes at spawn); RSS is their current resident memory, summed.
- Every sample carries a unit, a timestamp, a `scope`, and `available: bool` with a reason.
- **Auto-throttle:** if host PSI or CPU is critical, sample every 15–30 s and skip guest metrics.

### 6.6 Transport and buffering
- Implements ADR-0005: hello/welcome, journal and telemetry streams, cumulative acks, server-authoritative resume.
- Ping every 15 s; dead after 45 s. Reconnect with full-jitter backoff (backon), capped at 30 s.
- HTTPS batch-POST fallback (`/api/v1/agent/batch`) after N failed WS upgrades.
- **Outbox:** the journal spools to disk (64 MiB default) and is the source of truth; a small in-memory send window (1024 events / 2 MiB) is read back from it, so memory does not grow with the backlog (I10). When the spool or the window is full the agent stops storing event bodies and sequences one `journal.gap` per dropped run (I5); `journal.gap` and `run.ended` have a reserve and always fit. Telemetry: telemetry is a 2 MiB ring that coalesces 5 s samples into 1 min and then drops.
- **Uploads:** allowlisted artifact classes; at most 2 concurrent uploads, rate-capped; sha256 while streaming; `upload_request` → grant → PUT → `upload_complete`.

### 6.7 CI adapters (`fleet-agent::ci`)
- **Trait** `CiProvider { detect(), context() -> CiContext, oidc_token(audience) -> Option<Secret> }`.
- **v1 implementations:** `GitHubActions` (env vars + `ACTIONS_ID_TOKEN_REQUEST_*`) and `Generic` (`FLEET_*`).
- **Job key:** `FLEET_JOB_KEY` (docs recommend `${{ job.check_run_id }}`) → OIDC `check_run_id` claim → `GITHUB_JOB` + hash of `FLEET_MATRIX` (with an `identity_quality=weak` flag).
- **Trust level:** fork PRs (no OIDC token) → offline mode with a local bundle at `$RUN/fleet-bundle.tar.zst`.

---

## 7. Server design: `fleet` (research 03, 05, 06)

### 7.1 Modules (`crates/fleet-server/src`)
```
main.rs (clap: serve | migrate | check-config | healthcheck | create-admin)
config.rs  app.rs (routers: app origin + usercontent origin)  state.rs  telemetry.rs
auth/      oidc.rs  session_store.rs  gha_oidc.rs (JWKS cache)  api_keys.rs  agent_tokens.rs  extractors.rs
ingest/    agent_ws.rs  batch_http.rs  batcher.rs  ack.rs  limits.rs
live/      hub.rs (projections, versions)  bus.rs (EventBus: InProcess | PgNotify)  browser_ws.rs
api/       projects pipelines jobs workers devices executions flows artifacts analytics admin health openapi
artifacts/ broker.rs  verify.rs  links.rs  usercontent.rs
jobs/      leader.rs (advisory lock)  sweeps.rs (liveness, retention, partitions, sessions)  queue.rs
           (SKIP LOCKED)  finalize.rs  rollups.rs
web.rs     (fleet-web-assets, SPA fallback, CSP)
```

### 7.2 Ingestion path
socket → frame decode + size limits → validate (garde) → **one transaction per batch**:
1. Fence the epoch.
2. Dedup on `seq` ≤ `committed_seq`.
3. Apply claims and entities with `ON CONFLICT` plus an ownership check.
4. Bump entity `rev`s and pipeline versions.
5. Advance `committed_seq`.

Then **COMMIT → ack → hub update → broadcast**. Telemetry is buffered and flushed every 10–30 s with UNNEST/COPY, then `telemetry_ack`. Heartbeats never touch the DB.

### 7.3 Live state for browsers
REST snapshot (with `version`) → WS subscribe (scoped by project or pipeline) → ordered deltas with `version`. On `Lagged` or a version gap, the server sends `resync` and the client re-fetches the snapshot. After a restart, the hub rebuilds from Postgres. Liveness timers restart from `max(last_seen, boot)` + grace.

### 7.4 State machines and evidence (in `fleet-core`)
Implement exactly the transition tables in research 05 §6 and the claim lattice in §7:
- Worker liveness: online → stale → disconnected → completed.
- Agent run.
- Job: lifecycle and outcome kept separate.
- Suite, flow and device session.
- The JUnit reconciler from 05 §8: positional per shard, then `file=`, then name.
- Each is a pure function with property tests: order-independence, idempotency, never-passed-from-inferred.

### 7.5 Auth and authorization
- **Browser:** OIDC code + PKCE. Cookie session (HttpOnly, Secure, SameSite=Lax) stored in Postgres. CSRF token on mutating REST calls. **Group → role mapping** is configured per project (Viewer, Maintainer, Admin). Deny access when group claims are missing; support Entra app roles.
- **Agents:** `POST /api/v1/agent/token` exchanges a GitHub OIDC JWT (audience `maestro-fleet` by default) for an opaque, hashed token with a 2 h TTL. It is scoped to {project, job, trust level} and write-only.
  - Trust policies match `repository_id` and `repository_owner_id`, optionally `ref` and `event_name`.
  - **API-key fallback:** keys look like `mf_live_<random>`, stored as SHA-256 hashes, scoped to one project, write-only.
- **Policy:** a hand-rolled `authorize(principal, action, resource) -> Decision`, with a test table of every role × action.

### 7.6 Artifacts
Upload is relayed through the server by default; presigned direct upload is optional. Then:
1. **Verify:** size, sha256, magic bytes (`infer`), archive safety (no zip bombs, no path traversal).
2. **Store:** content-addressed per project.
3. **Retention:** promoted at finalization.
4. **Downloads:** short-lived signed links.
5. **HTML reports:** served only from the usercontent origin with `Content-Security-Policy: sandbox`, `X-Content-Type-Options: nosniff`, `Content-Disposition` as appropriate.

### 7.7 Background jobs (leader only)
- Liveness sweep (10 s).
- Finalization of quiet jobs.
- Partition create and drop (daily).
- Rollups (1 min and 1 h; late-data aware).
- Retention and blob GC (grace period).
- Session cleanup.
- Audit-log retention.

---

## 8. Protocol (`fleet-protocol`; ADR-0005; research 05 §3–5)

- **Envelope:** `{ op, … }` frames over the `fleet.v1+json` subprotocol. Journal events: `{ seq, type, v, at: {wall, mono_ms}, subject: {ids…}, provenance, body }`.
- **Event families:**
  - `run.*`: started, signal_received, ended.
  - `plan.*`: declared, observed.
  - `device.*`: session_started, state_changed, lost.
  - `suite.*`, `shard.*`.
  - `flow.started`, `flow.claim` (the outcome as a claim with provenance), `step.*`.
  - `report.*`: parsed, absent, invalid.
  - `artifact.*`: registered, uploaded, rejected.
  - `agent.*`: warning, parser_drift, self_metrics.
  - `journal.gap`.
- **Compatibility:**
  - Unknown `type` → stored raw and ignored by projections.
  - Unknown fields → ignored.
  - Breaking change → new `v` for that type, or a new subprotocol.
- **Generated artifacts:** JSON Schema files under `docs/src/protocol/schema/`. A CI check fails if they are stale.
- **Golden tests:** fixtures of every frame type, round-tripped.

---

## 9. Data model (`fleet-store`; research 05 §9–13)

**Migrations** (forward-only; each reviewed against the DDL sketches in research 05 §9):

| Migration | Contents |
|---|---|
| `0001_tenancy` | `projects`, `users`, `role_bindings`, `group_mappings`, `publisher_trust_policies`, `api_keys`, `sessions`, `audit_events` |
| `0002_ci_identity` | `pipeline_runs`, `pipeline_attempts`, `jobs`, `agent_runs` (`committed_seq`, `epoch`), `connections` |
| `0003_execution` | `workers`, `device_profiles`, `device_sessions`, `suite_executions`, `shards`, `flow_definitions`, `flow_executions`, `flow_claims` (with **CHECK: inferred provenance ∌ passed**), `step_executions` (partitioned) |
| `0004_journal` | `agent_events` (partitioned by `run_day`), `ingest_gaps`, `ingest_rejections` |
| `0005_telemetry` | `host_samples`, `device_samples` (partitioned by time), `host_rollup_1m`, `host_rollup_1h`, `device_rollup_*` |
| `0006_run_evidence` | `agent_runs.evidence` (jsonb: the run's evidence sources, from `run.started`) |
| `0007_analytics` | `condition_fingerprints`, `failure_fingerprints`, flaky views |
| `0008_artifacts` | `blobs` (project, sha256), `artifacts`, `retention_classes`, `jobs_queue` |

**Retention:** 7 d raw 5 s samples; 90 d at 1 min; 1 y for hourly and daily aggregates. The raw journal is kept 14 d. All of it is configurable.

---

## 10. Dashboard (`fleet-web`, `fleet-ui`; spec §8)

### 10.1 Architecture
- **Routing:** `leptos_router`.
  - `/` overview
  - `/p/:project/pipelines/:id`
  - `/workers/:id`, `/devices/:id`
  - `/flows/:execution_id`
  - `/history`, `/compare`, `/admin/*`
- **Data:**
  - REST via `gloo-net` with types from `fleet-protocol`; no hand-written DTOs.
  - Live updates via a single multiplexed browser WS; subscriptions follow the route.
  - A `LiveStore` (§5.4 pitfalls) applies deltas, tracks the version, and re-snapshots on a gap.
- **Provenance everywhere:** `fleet-ui::Provenance` badge and tooltip: observed, provisional, final, derived, inferred, stale, missing. Stale data greys out after a freshness threshold.
- **Security:**
  - The SPA holds no tokens; it uses the session cookie only.
  - Strict CSP: `default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' blob: <usercontent>; connect-src 'self'; frame-ancestors 'none'`.
  - All user text is rendered as text nodes.

### 10.2 `fleet-ui` component backlog (build in this order)

| Order | Components |
|---|---|
| 1 | Tokens and themes, Layout, Nav, Button, Badge/StatusPill, Provenance, Card, Stat tile |
| 2 | Table (sort, filter, column picker), VirtualList, Pagination, EmptyState, Skeleton |
| 3 | Dialog (native `<dialog>`), Popover/Menu (Popover API), Tabs, Tooltip, Toast |
| 4 | Combobox (APG pattern), DateTimeRange (native inputs), Filter bar with URL-synced state |
| 5 | Sparkline, TimeSeriesChart (SVG + LTTB; canvas fallback; crosshair, tooltips, accessible data table toggle) |
| 6 | SwimlaneTimeline (canvas; devices × time; flows as bars coloured by outcome and provenance; zoom, pan, keyboard navigation; synced cursor with charts for metric correlation; table alternative) |
| 7 | LogViewer (virtualized, ANSI via `vte`, search, jump-to-step, line anchors), ScreenshotViewer (zoom, step strip) |
| 8 | StepTree (nested `runFlow`, retries, durations) |

### 10.3 Screens, by milestone
- **M4:** A (Overview), B (Pipeline detail with timeline + live activity), C (Worker/device detail with correlated charts), D (Flow investigation).
- **M5:** E (History), F (Compare, with mismatch explanations).
- **M6:** G (Admin: projects, trust policies, API keys, role mappings, retention, artifact policy, ingestion health, audit).

---

## 11. Tooling, CI and release

### 11.1 `cargo xtask`
| Command | Does |
|---|---|
| `setup` | Install pinned tools: nextest, deny, about, sqlx-cli, trunk, wasm-bindgen-cli (matching version), zigbuild, mdbook, Tailwind standalone (checksum-verified); add rustup targets |
| `ci` | fmt --check → clippy (`-D warnings`, all targets) → lint (headers, I8, I9, dependency rules) → deny → nextest → sqlx prepare --check → schema-drift check → web build |
| `web [--release]` | `trunk build` for `fleet-web` → `crates/fleet-web/dist` |
| `sqlx` | Start a throwaway PG 18 (docker), migrate, `cargo sqlx prepare --workspace` |
| `fixtures import <dir>` | Normalize a captured Phase 0 run into `fixtures/maestro/<ver>/<scenario>/` (redacting) |
| `dist` | Local release build of all targets |
| `licenses` | `cargo about generate` → `THIRD_PARTY_LICENSES.html` |
| `compose up/down` | Run the reference deployment locally |

### 11.2 GitHub workflows
| Workflow | Trigger | Content |
|---|---|---|
| `ci.yml` | PR, push to main | `cargo xtask ci` on ubuntu-latest (Postgres 18 service); macOS arm64 job for agent build + tests; wasm build |
| `cla.yml` | PR, comments | CLA Assistant Lite (pinned SHA) |
| `release.yml` | Tag `v*` | cargo-dist: agent for linux-musl x86_64/aarch64 and macOS arm64, server for linux x86_64/aarch64; SHA-256, GitHub artifact attestations, minisign signatures; THIRD_PARTY_LICENSES |
| `image.yml` | Tag `v*` | Multi-arch `ghcr.io/<owner>/fleet` (native arm64 runner), SBOM, attestation |
| `nightly-emulator.yml` | Nightly + manual | Linux x64 + KVM, `reactivecircus/android-emulator-runner`, Maestro pinned, `maestro download-samples` flows; run wrapped by the agent against a Compose stack in the job; assert the reconciled results; upload fixtures as artifacts |
| `audit.yml` | Weekly | cargo-deny advisories, `cargo outdated` report |

Pin every third-party action by commit SHA. Use `permissions:` least-privilege per workflow.

### 11.3 Distribution
- **Agent:** GitHub Releases, plus **served by the Fleet server** at `/downloads/agent/<version>/<target>` (binaries baked into the image), plus a composite action in `examples/` (and later a `setup-maestro-fleet` action repo). Each release's license file states that the agent is covered by the same license.
- **Server:** container image plus the Compose bundle.

---

## 12. Milestones

Effort assumes one experienced Rust engineer part-time. Treat estimates as rough.

### M0: Repository bootstrap (1–2 days). **The executing agent does this now.**
1. Create the repo `<owner>/maestro-fleet` (or the chosen name, O1) on the owner's personal GitHub account. **Private until the owner flips it public.**
2. Import the existing material unchanged: `docs/spec-v1.0.md`, `docs/PLAN.md` (this file), `docs/decisions/*`, `research/*` (including `research/fixtures/`).
3. Licensing and governance files from §2: `LICENSE.md` (verbatim PolyForm Internal Use 1.0.0 + Required Notice), `LICENSING.md`, `CLA.md`, `CLA-CORPORATE.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `.github/FUNDING.yml`.
4. Workspace skeleton (§4): every crate compiles with a stub `lib.rs`/`main.rs`, SPDX headers, `publish = false`, `license-file`. Workspace lints: `unsafe_code = "deny"` except in `fleet-agent::sys` (documented), `clippy::pedantic` warn.
5. `xtask` with `setup`, `ci`, `lint`, `web`.
6. `deny.toml` and `about.toml` per §2.6.
7. `ci.yml` and `cla.yml` green on an empty PR.
8. `fleet-web` hello page built by Trunk and embedded and served by `fleet serve` at `/` with the CSP from §10.1. This proves the wasm → embed → axum chain.
9. mdBook skeleton under `docs/`, with the README links updated.
10. GitHub settings to recommend to the owner (they can't all be set through the API): branch protection on `main` (required checks, linear history), private vulnerability reporting on, Discussions on, Sponsors profile, topics.

**Exit:** `cargo xtask ci` passes locally and in CI; `fleet serve` shows the hello page; license files are present and verbatim.

### M1: Phase 0, Maestro integration proof (1–2 weeks; needs an emulator machine)
- Linux x64 + KVM, or a self-hosted Apple Silicon machine; Maestro pinned to the current 2.12.x.
- Scenarios:
  - 1 pass; 1 fail; a multi-flow suite;
  - `--shard-split 2`; `--shard-all 2`;
  - `executionOrder` with `continueOnFailure: false`;
  - nested `runFlow` and the `retry` command;
  - two emulators of the same AVD;
  - device killed mid-run;
  - SIGINT, SIGTERM, SIGKILL;
  - no device; bad flag;
  - a planted secret in `-e` and in `MAESTRO_*`.
- Capture for each scenario: byte-exact stdout and stderr, `maestro.log` with default **and** machine patterns, an inotify/fsevents trace with timestamps, the bundle tree, JUnit/HTML, the exit code, and the process tree.
- Use `cargo xtask fixtures import` to normalize into `fixtures/maestro/2.12.x/…`.
- **Answer the 17 questions in research 01 §15.** Write `research/08-phase0-results.md` and update ADR-0003 from Proposed to Accepted (or record the fallback).
- Run the same fixtures through ReportPortal and Allure 3 (1–2 days) for the build-vs-buy record (ADR-0009).
- Open the upstream discussion or PR for `--events` (O3).

**Exit (gate):** each progress state is classified *observable / inferred / unavailable* with fixture evidence; the device-correlation rule is confirmed.

### M2: Agent (3–5 weeks)
- `fleet-maestro` parsers + `inspect`, snapshot-tested against every M1 fixture.
- Supervisor with fake-Maestro tests: exit codes 0, 1, 2, signals, huge output, binary output, a hung observer, a crashed observer.
- Observer: lanes A–D, state actor, reconciliation (E), metrics, adb client, redaction.
- Outbox + spool + transport against a **mock server** in `fleet-loadgen`'s test harness. `flush`, `check`, `selftest`.
- Release pipeline (cargo-dist) producing signed binaries.

**Exit:**
- I1, I2, I5, I6 and I10 are proven by tests.
- An overhead benchmark (suite duration with and without the agent, on the M1 machine) shows < 1% CPU and a duration delta within noise.
- A real CI run streams to the mock server without changing the result.

### M3: Server core (4–6 weeks)
- Migrations 0001–0005, ingestion transaction, acks/resume/fencing, hub + browser WS, liveness sweeps, REST snapshots, OpenAPI, agent auth (GitHub OIDC + API keys), `migrate`, `check-config` and `healthcheck`, Compose (Postgres 18 + Garage + Caddy).
- `fleet-loadgen` scenarios: 50 agents × 4 devices; reconnect storms; duplicates and reordering; a server restart mid-run; overlapping job names.

**Exit:** concurrent agents publish without corrupting state; a restart loses no committed events; acceptance criteria 3, 10, 11 and 15 pass in automated tests.

### M4: Dashboard core (4–6 weeks; can overlap M3 after the protocol freezes)
- `fleet-ui` orders 1–8, then screens A–D.
- Browser OIDC login (BFF).
- E2E: watch a loadgen-driven run live, survive a server restart (re-snapshot), see stale and disconnected states.

**Exit:** a developer can follow a full run without CI logs (spec Phase 3 gate); acceptance criteria 2, 4, 5, 6 and 14 pass.

### M5: Evidence and history (3–4 weeks)
- Artifact broker + verification + usercontent origin.
- Screenshot and log viewers.
- Retention promotion and GC.
- Failure fingerprints, condition fingerprints and flaky v1 (ADR-0009).
- Screens E and F.

**Exit:** a failed run can be diagnosed after the runner is gone (acceptance criteria 7, 8, 9).

### M6: Hardening and handover (3–4 weeks)
- RBAC per project.
- Screen G and audit.
- Security review against the research 06 threat model, including malicious logs and HTML, zip bombs and untrusted-PR behaviour.
- Load tests.
- Backup/restore runbook (pg_dump + Garage).
- Operator guide; runner guide (KVM, Apple Silicon, no hosted macOS arm64).
- Upgrade procedure.
- v1.0.0 release.

**Exit:** all 15 spec acceptance criteria pass; IT can deploy from the docs alone (spec Phase 5 gate).

### M7+: Optional
Notifications (webhook, Slack), OTLP export, GitLab and Buildkite adapters, the fork-PR relay (O2), iOS simulators, the Leptos 0.9 upgrade, AI summaries (only with approval).

---

## 13. Testing strategy (summary)

| Level | What | Tools |
|---|---|---|
| Unit/property | Claim lattice, state machines, reconciliation, redaction, LTTB, policy table, outbox bounds, spool CRC | proptest, insta |
| Parser | Every fixture × every parser; drift (mutated lines) never yields outcomes | insta snapshots |
| Agent integration | Fake-Maestro binary scripted from fixtures (timed replay of stdout, log and files, then exit or signal) | assert_cmd, nextest |
| Server integration | Ingest, idempotency, fencing, restart recovery, auth boundaries | sqlx::test, testcontainers, axum-test |
| Load/resilience | Reconnect storms, duplicates, reordering, clock skew, full buffers, giant logs | fleet-loadgen |
| UI | Component logic (wasm-bindgen-test); E2E flows and accessibility smoke | thirtyfour + chromedriver, vendored axe-core (tests only) |
| Real world | Nightly emulator job; Maestro version matrix (current, N−1) | GitHub Actions + KVM |
| Security | Header and CSP assertions, XSS corpus in flow names and logs, archive bombs, authZ matrix | custom tests |

**Compatibility rule:** a new Maestro minor version needs fixtures before support is claimed. Unknown versions run in "degraded" mode.

---

## 14. Documentation (mdBook, `docs/src`)

- **User guide:** wrapping Maestro in CI, the GitHub Actions example, reading provenance badges, investigating failures.
- **Runner guide:** emulator hosts (Linux KVM / Apple Silicon), proxy and CA settings, offline mode, `check`.
- **Operator guide:** Compose deployment, config reference, SSO set-up (Okta, Entra, Google, Keycloak, Authentik; group mapping and Entra overage), usercontent domain, backup/restore, upgrades, retention.
- **Protocol reference:** generated JSON Schema plus semantics (ADR-0005).
- **Security model:** threat model from research 06 §6; secret-handling guidance ("never pass secrets via `-e`").
- **Licensing FAQ:** a mirror of `LICENSING.md`.

---

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| `maestro.log` is not flushed live (M1) | Step-level live progress lost | Fall back to per-flow progress; push the upstream `--events` reporter; the UI already handles both levels |
| Maestro output or layout changes (2.7 changed everything) | Parsers break | Version profiles, degraded mode, nightly version matrix, fixture-first rule |
| Leptos 0.x churn (0.9 RC now) | Upgrade cost | Pin `=0.8.23`; isolate the framework in `fleet-ui`/`fleet-web`; upgrade by ADR |
| Building accessible UI primitives in-house | Time, a11y bugs | Native elements first; APG patterns; axe in E2E; a small, fixed component list |
| Employer IP claim | Ownership dispute | §2.5 written confirmation before code |
| A non-OSI license deters contributors and some company legal teams | Fewer contributors | Clear `LICENSING.md`; internal use is free; CLA explained; option of a commercial/other license on request |
| Postgres write rate | Ingest latency | Batch per socket, telemetry checkpoints, COPY/UNNEST, partitions |
| Corporate proxies, CAs, WS blocking | Agent can't publish | CONNECT proxy, custom CA, HTTPS batch fallback, `check` diagnostics, fail-open |
| GitHub cancel window (10 s) | Lost finalization | Pre-emptive `run.signal_received`, spool + `flush` post-step |
| Name confusion with Maestro (mobile.dev) | Trademark or brand issue | Decide O1 before going public; describe it as "for Maestro"; centralised naming (§4.1) |

---

## 16. Definition of done for v1.0.0

- Spec acceptance criteria 1–15 (§16 of the spec) demonstrated by automated tests or a recorded demo.
- Invariants I1–I10 have tests in CI.
- Release artifacts are signed and attested and include THIRD_PARTY_LICENSES.
- Docs are complete enough for an IT team to deploy, operate and upgrade without the author.
- LICENSE, CLA and FUNDING are in place; `LICENSING.md` has been reviewed by the owner (ideally a lawyer).
