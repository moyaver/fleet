# 03 — Fleet Server in Rust: framework, libraries and internal architecture

**Status:** research, 2026-11-03
**Scope:** the Fleet Server as one deployable Rust binary covering HTTP REST, agent WebSocket ingestion, browser live-update WebSockets, artifact upload brokering, auth, and background jobs (retention, liveness/heartbeat expiry, finalization).
**Out of scope:** the detailed wire protocol and database schema, which another researcher covers. They come up here only where they affect the server's internal design.
**Spec sections reviewed:** 2B, 5, 6, 7, 10, 11, 13 (plus 14–16 for the gates and tests).

---

## 0. TL;DR

| Area | Recommendation |
|---|---|
| HTTP framework | **axum 0.8.9** + tower 0.5.3 + tower-http 0.7.1 on tokio 1.54. Plan for a 0.8 → 0.9 migration: breaking changes are already on axum `main`, not yet released. |
| Database | **sqlx 0.9.0** (Postgres only), compile-time-checked `query!` with an offline `.sqlx/` directory committed for CI, embedded `sqlx::migrate!`, `PgListener` for LISTEN/NOTIFY, `PgAdvisoryLock` for leader election. |
| Live state | An in-process projection (`Hub`): per-pipeline state behind a short-held lock, **with the version counter persisted in Postgres**. `broadcast` channels carry ordered deltas and `watch` channels carry coalesced "latest value" tiles. On `Lagged` the server sends a resync and the client re-snapshots. All of this sits behind an `EventBus` trait so Postgres NOTIFY (or NATS) can be added later without changing the core. |
| Background jobs | v1 needs no job framework. Periodic sweeps are tokio `interval` loops run only by the advisory-lock holder. Retryable work (finalization, object deletion) goes in a small `jobs` table using `FOR UPDATE SKIP LOCKED`. `apalis-postgres` 1.0 is the upgrade path. |
| Object storage | **object_store 0.14.2**. Verified: path-style S3 with a custom endpoint, presigned PUT/GET, presigned *multipart part* URLs, signed checksum/content-type headers, and a `LocalFileSystem` backend. Local FS has no presigning, so the server brokers those uploads through its own HMAC-signed upload URLs. |
| Browser SSO | **openidconnect 4.1.0** (auth code + PKCE + nonce) + **tower-sessions 0.15** with a ~60-line **custom Postgres `SessionStore`**. The published `tower-sessions-sqlx-store` 0.15.0 is incompatible with sqlx 0.9 / tower-sessions 0.15 (verified). |
| Agent auth | GitHub Actions OIDC JWT verified with **jsonwebtoken 11.1** (a crypto backend feature must be selected), using a JWKS cache that refetches on unknown `kid`. The JWT is exchanged for an opaque, hashed, short-lived Fleet token scoped to project + session. Fallback: prefix-identifiable API keys hashed with SHA-256, not argon2. |
| Authorization | Hand-rolled policy module (`Role` enum + `authorize(principal, action, project)`). Skip casbin. Oso's OSS library is deprecated. Keep cedar-policy 4.x in reserve in case ABAC is ever needed. |
| API contracts | **utoipa 6.0 + utoipa-axum 0.3 + utoipa-scalar 0.4** for REST OpenAPI. **schemars 1.2** for the WS protocol JSON Schema. **garde 0.23** + **nutype 0.8** for validation and newtypes. **ts-rs 12** for TypeScript. All shared types live in a dependency-light `fleet-protocol` crate. |
| Frontend serving | **memory-serve 2.4** (or rust-embed 8.13) for the SPA, with an `index.html` fallback, immutable caching of hashed assets, and a strict CSP. Maestro HTML reports are served **only from a separate "usercontent" origin**, with `Content-Security-Policy: sandbox` and `nosniff`, and never from the app origin. |
| Ops | figment config, a `CancellationToken` + `TaskTracker` drain for WebSockets, `/livez` + `/readyz`, tracing JSON logs, optional OTLP, and metrics + metrics-exporter-prometheus. tower_governor for coarse rate limits. **Explicit WS size limits: tungstenite's default max message size is 64 MiB.** |
| Testing | `#[sqlx::test]`, testcontainers (Postgres + MinIO), axum-test 21, and a **custom Rust load generator** built on the agent's own protocol client. |
| Container | cargo-chef + `SQLX_OFFLINE=true`, `gcr.io/distroless/cc-debian12` (glibc). Use native arm64 builders, or cargo-zigbuild, rather than QEMU. Expected image size ~35–40 MB. |
| Capacity | One instance should handle hundreds to low thousands of agent sockets on CPU and memory. **Postgres commit rate is the bottleneck**, so micro-batch lifecycle events per socket, keep heartbeats out of the DB, and persist metrics as checkpoints written with UNNEST/COPY. |

### Things that change or sharpen the spec

1. **§6 "Backend stack recommendation"** becomes Rust/axum/sqlx (above). The spec's "typed runtime validation" becomes serde + garde/nutype, and "OpenAPI documentation" becomes utoipa.
2. **§6 Redis for fan-out**: not needed. The first horizontal-scaling step is Postgres LISTEN/NOTIFY behind an `EventBus` trait. NATS is the second step if NOTIFY's global commit serialization ever matters. This is consistent with §11 ("no Redis without demonstrated need").
3. **§6 Live state / §13 Backend restart**: pipeline **versions should be persisted in Postgres** (incremented inside the ingestion transaction), not held only in memory. Versions then survive restarts and stay consistent across instances later.
4. **§5 Reliability**: separate *durable* acks (lifecycle events, acked per sequence after commit) from *accepted* acks (metric samples, persisted at the 30–60 s checkpoint). Otherwise each 5 s metric costs a commit. The protocol researcher should decide the exact wire form.
5. **§6 State expiration / §13 Backend restart**: liveness timers must restart from `max(last_seen_at, server_boot_at)` plus a reconnect grace period. Otherwise a server restart marks every running job interrupted.
6. **§10 "Prevent HTML reports from executing untrusted content in the application's origin"**: make this concrete as a **separate usercontent hostname** (requires DNS and a certificate from IT) plus `CSP: sandbox`. That adds a deployment requirement to §11.
7. **§10 CI publisher auth**: trust policies should match on immutable IDs (`repository_id`, `repository_owner_id`), never by parsing `sub`. GitHub changed the `sub` format for repositories created after 2026-07-15 (verified in GitHub docs).
8. **§10 Network "corporate certificates supported"**: several Rust TLS defaults use bundled webpki roots and **ignore the OS trust store**. Feature flags need specific choices (see §9.6).
9. **§11**: provide a `fleet-server migrate` subcommand alongside auto-migrate-on-start, plus a `fleet-server check-config` subcommand.

---

## 1. Verification legend

Each claim is marked as one of:

- **[V-crates]**: checked on crates.io API on 2026-11-03 (version, release date, dependency requirements).
- **[V-src]**: checked by reading the published crate source/README/CHANGELOG (via the cargo registry or raw.githubusercontent.com).
- **[V-poc]**: compiled and/or ran in the throwaway PoC at `/tmp/claude-0/-home-user/35b53bb7-a211-5ee3-8f5b-0d3a8129de43/scratchpad/server-poc` (cargo 1.97, edition 2024).
- **[V-doc]**: checked in official external docs (GitHub, PostgreSQL, MDN).
- **[I]**: inferred from experience or general knowledge, not re-verified today. Treat as a hypothesis.

Limits on what was verified:
- The GitHub REST API was **not reachable** from this environment, so stars, open issues and last-commit dates could not be checked. "Maintenance status" is judged from crates.io release cadence and download trend, plus README/CHANGELOG content.
- There was **no Docker daemon**, and a local Postgres could not be started inside the sandbox. Testcontainers, `#[sqlx::test]` execution, real MinIO presign round-trips and the DB throughput numbers are therefore **not** runtime-verified.

### What the PoC proved [V-poc]

All of the following compile together in one crate with no version conflicts that block the build (491 crates in the tree):

- axum 0.8.9 WS (`max_message_size`, `max_frame_size`, `on_upgrade`, split sink/stream).
- tower-http 0.7.1 layers (Trace, RequestBodyLimit, Timeout, SetResponseHeader).
- sqlx 0.9.0: `migrate!`, `PgListener`, `PgAdvisoryLock::try_acquire`, UNNEST insert, `copy_in_raw`, transaction, `#[sqlx::test]`.
- object_store 0.14.2: S3 builder (path-style), `Signer::signed_url_opts`, `MultipartStore::create_multipart`, `LocalFileSystem`.
- openidconnect 4.1.0 discovery + PKCE auth URL.
- jsonwebtoken 11.1 `JwkSet` / `DecodingKey::from_jwk` / `Validation`.
- tower-sessions 0.15 `SessionManagerLayer` with a custom `PgSessionStore`.
- utoipa 6 + utoipa-axum 0.3 `OpenApiRouter` + utoipa-scalar 0.4.
- schemars 1.2, garde 0.23, ts-rs 12 on one shared struct.
- rust-embed 8.13 SPA fallback, figment env+toml, metrics-exporter-prometheus 0.18, tower_governor 0.8, dashmap 6, axum-test 21.

Runtime tests that passed:
- An end-to-end WS test: an axum server and a tokio-tungstenite client receive a versioned delta published through the Hub, and the snapshot version matches.
- A `broadcast` lag test: capacity 4, 10 sends, `Lagged(6)` returned, then the oldest retained value.
- Offline S3 presigning: path-style `http://minio:9000/fleet/key?X-Amz-…` URLs, with `X-Amz-SignedHeaders=content-type;host;x-amz-checksum-sha256` when headers are bound, and `partNumber`/`uploadId` included in part URLs.
- ts-rs generated `bindings/PipelineSummary.ts`.

Build data [V-poc] on 4 vCPU:

| Build | Time |
|---|---|
| Cold release (`lto="thin"`, `codegen-units=4`) | **10 min 48 s** |
| Incremental release after touching `main.rs` | **2 min 25 s** (thin LTO dominates) |
| Incremental debug | **11 s** |

The **stripped binary is 13.4 MB** and links dynamically only against glibc/libgcc.

---

## 2. Recommended crate list

Versions are the latest stable on crates.io as of 2026-11-03 [V-crates]. "Maintenance" is judged from release cadence and is described in words.

| Purpose | Crate | Version | Released | Maintenance | Notes |
|---|---|---|---|---|---|
| Async runtime | `tokio` | 1.54.1 | 2026-10-21 | Very active (tokio-rs) | `full` features in the server; `tokio-util` 0.7.20 for `CancellationToken`, `TaskTracker`. |
| HTTP framework | `axum` | 0.8.9 | 2026-04-14 | Active; 0.9 breaking changes on `main` [V-src] | `ws`, `macros` features. WS is built on tokio-tungstenite 0.29. |
| Extras | `axum-extra` | 0.12.6 | 2026-04-14 | Active | `TypedHeader`, cookie jars if needed. |
| Middleware | `tower` / `tower-http` | 0.5.3 / 0.7.1 | 2026-01 / 2026-08 | Active | Works with axum 0.8.9 [V-poc]. Note: axum's own optional dev deps still reference tower-http 0.6.8. |
| DB | `sqlx` (+ `sqlx-cli`) | 0.9.0 | 2026-05-21 | Active; repo moving to `transact-rs` org [V-src] | MSRV 1.94. Features: `runtime-tokio`, `postgres`, `macros`, `migrate`, `uuid`, `time`, `json`, **`tls-rustls-ring-native-roots`**. |
| Concurrent map | `dashmap` | 6.2.1 | 2026-05-17 | Active | Projection index; alternatives `papaya` 0.2.5, `scc` 3.8.8. |
| Object storage | `object_store` | 0.14.2 | 2026-09-15 | Active (Apache Arrow) | `aws` feature. Presign with signed headers/query [V-poc]. |
| OIDC (browser) | `openidconnect` | 4.1.0 | 2026-10-31 | Active (ramosbugs) | Pulls `oauth2` 5.0 → **reqwest 0.12** (a second reqwest in the tree). |
| Sessions | `tower-sessions` | 0.15.0 | 2026-02-01 | Moderate | Use with a custom PG store (§8.1). **Do not use** `tower-sessions-sqlx-store` 0.15.0 (pins sqlx 0.8 + core 0.14) [V-crates]. |
| JWT verify | `jsonwebtoken` | 11.1.0 | 2026-09-16 | Active (Keats) | Must enable `aws_lc_rs` **or** `rust_crypto` [V-src]. |
| Hashing | `sha2` / `subtle` / `rand` | 0.11.1 / 2.6.1 / 0.10.3 | 2026 | Active (RustCrypto) | SHA-256 for API keys and artifacts; constant-time compare. `argon2` 0.6.0 is not needed (§8.3). |
| Authorization | none (hand-rolled) | — | — | — | `cedar-policy` 4.14.1 (2026-11-02, AWS, very active) only if ABAC becomes a requirement. |
| OpenAPI | `utoipa` / `utoipa-axum` / `utoipa-scalar` | 6.0.0 / 0.3.0 / 0.4.0 | 2026-09-22 | Active | utoipa 6 adds OpenAPI 3.2 support [V-src]. `utoipa-swagger-ui` 10.0.2 is an alternative UI. |
| JSON Schema | `schemars` | 1.2.3 | 2026-10-12 | Active | `uuid1` feature for `Uuid` [V-poc]. Used for the WS protocol schema. |
| Validation | `garde` | 0.23.0 | 2026-05-23 | Active | Derive-based; `validator` 0.21 is the alternative. |
| Newtypes | `nutype` | 0.8.0 | 2026-09-20 | Active | `ProjectSlug`, `FlowPath`, `Sha256Hex`, etc. with sanitize + validate. |
| TS types | `ts-rs` | 12.0.1 | 2026-01-31 | Active | `#[ts(export)]` [V-poc]. `specta` 2 is still RC; `typeshare` 1.0.5 is CLI-based. |
| Static assets | `memory-serve` | 2.4.0 | 2026-09-17 | Active (Tweede golf), small user base | Build-time brotli, ETag, SPA fallback. `rust-embed` 8.13.0 is the popular alternative [V-poc]. |
| Config | `figment` | 0.10.19 | 2024-05-17 | Stable / slow releases | Errors report which source a bad value came from. `config` 0.15.27 is the active alternative. |
| Logging | `tracing` / `tracing-subscriber` | 0.1.44 / 0.3.23 | 2025-12 / 2026-03 | Active | `json`, `env-filter`. |
| OTel (optional) | `opentelemetry` / `-otlp` / `tracing-opentelemetry` | 0.33.1 / 0.33.1 / 0.34.0 | 2026-09/10 | Active, still 0.x (churn) | Put behind a cargo feature `otel`. |
| Metrics | `metrics` / `metrics-exporter-prometheus` | 0.24.6 / 0.18.3 | 2026-05 / 2026-04 | Active | `axum-prometheus` 0.10.1 adds HTTP RED metrics (uses tower-http 0.7). |
| Rate limit | `tower_governor` | 0.8.0 | 2025-08-14 | Slow but compatible with axum 0.8 [V-poc] | Coarse per-IP/per-key limits. |
| Jobs (later) | `apalis` / `apalis-postgres` | 1.0.0 / 1.0.0 | 2026-10-12 | Active, **just reached 1.0** | Upgrade path only. Its `license-file` is MIT but crates.io shows "non-standard", so add a cargo-deny exception. |
| Errors | `thiserror` / `anyhow` | 2.0.21 / 1.0.104 | 2026 | Active | `thiserror` in libraries; `anyhow` only in `main`/CLI. |
| IDs / time | `uuid` (v7) / `time` | 1.29.0 / 0.3.55 | 2026 | Active | UUIDv7 for server-generated IDs. Use `time` to match tower-sessions. |
| HTTP client | `reqwest` | 0.13.5 | 2026-09-08 | Active | `rustls` feature uses rustls-platform-verifier, which reads the OS trust store (good for corporate CAs) [V-src]. |
| Allocator (musl only) | `mimalloc` | 0.1.52 | 2026-05-22 | Active | Only if building musl/static. |
| Testing | `testcontainers` / `testcontainers-modules` | 0.28.0 / 0.15.0 | 2026-08 / 2026-02 | Active, **version skew** | modules 0.15 requires testcontainers ^0.27, so pin 0.27 when using modules [V-crates]. |
| Testing | `axum-test` | 21.1.0 | 2026-08-20 | Very active (22.0 RC) | `ws` feature [V-poc]. |
| Testing | `cargo-nextest` | 0.9.151 | 2026-11-03 | Very active | Test runner. |
| Build | `cargo-chef` | 0.1.78 | 2026-08-12 | Active | Docker layer caching. |
| Build | `cargo-zigbuild` | 0.24.0 | 2026-10-21 | Active | Cross-compiling arm64 without QEMU. `cross` 0.2.5 (2023) looks stale. |
| Supply chain | `cargo-deny` / `cargo-auditable` | 0.20.2 / 0.7.7 | 2026 | Active | License and advisory gate; SBOM embedded in the binary. |

**Rejected or not recommended** [V-crates unless noted]:
- `actix-web` 4.18.1: healthy, but see §3.
- `poem` 3.1.12 / `poem-openapi` 5.1.16: last release 2025-07-28, so slowing.
- `loco-rs` 1.2.0: lags the ecosystem, pinning tower-http ^0.6.8 and axum-extra ^0.10 [V-crates].
- `salvo` 1.1.0: active, smaller ecosystem.
- `diesel`/`diesel-async`, `sea-orm` 2.0.4: see §4.
- `aws-sdk-s3` 1.157.0, `rust-s3` 0.38.0, `opendal` 0.59.5: see §7.
- `axum-oidc` 0.6.0: 2025-01; 1.0 still in dev; ~6k recent downloads; Codeberg-hosted.
- `axum-login` 0.18.0: depends on tower-sessions **0.14**.
- `oso` 0.27.3: legacy OSS library deprecated [V-doc]; last release 2024-01.
- `casbin` 2.20.0: overkill.
- `aide` 0.15.1: 0.16 still alpha.
- `specta`: 2.0 still RC.
- `sqlxmq` 0.6.0: pins sqlx ^0.8 [V-crates].
- `underway` 0.2.0: ~4k recent downloads.
- `pgmq` 0.33.7: requires a Postgres extension, which conflicts with "bring your own Postgres".
- `envy` 0.4.2: unmaintained since 2021.

---

## 3. HTTP framework

### Options (state as of 2026-11-03)

| Framework | Version | Notes |
|---|---|---|
| **axum** | 0.8.9 (2026-04) | Built on tokio/hyper 1.12/tower. WS via `axum::extract::ws` (tokio-tungstenite 0.29) with configurable `max_message_size`, `max_frame_size`, `read_buffer_size`, `write_buffer_size`, `max_write_buffer_size`, and subprotocol negotiation (`requested_protocols`/`set_selected_protocol`, new in 0.8.9) [V-src]. It uses the whole tower/tower-http middleware ecosystem, and most of the auth/OpenAPI/static crates above target it first. ~150M recent downloads (the most of any Rust web framework). **Unreleased 0.9** on `main` includes: `serve` applies hyper's `header_read_timeout` by default; `serve` output type changes; nested-router fallback merging; `ListenerExt::limit_connections`; `MaxConnectionAge`; `tower-log` no longer default [V-src]. Expect a mechanical migration sometime in 2027. |
| actix-web | 4.18.1 (2026-10) | Mature and fast. Uses its own actor-ish runtime model (per-worker single-threaded runtimes, `!Send` handlers) and its own middleware trait, not tower. WS via `actix-ws` 0.4. utoipa and tower-sessions-style crates support it less uniformly. Its performance advantage over axum is irrelevant at our load. |
| poem (+ poem-openapi) | 3.1.12 (2025-07) | Nice OpenAPI-first ergonomics, but releases have stalled for 15 months. Too risky for a long-lived internal tool. |
| loco.rs | 1.2.0 (2026-09) | Rails-like on axum, with sea-orm, generators, a mailer, workers and opendal storage. Its conventions (sea-orm models, Tera views, JWT auth for users, its own config format and worker system) don't fit a WS-ingestion-heavy server with custom auth. It also lags core deps (tower-http 0.6, axum-extra 0.10) [V-crates], so we would always trail the axum ecosystem. |
| salvo | 1.1.0 (2026-11-02) | Active, batteries-included (OpenAPI, ACME, WebTransport). Much smaller community (~1M recent downloads vs ~150M for axum) and fewer third-party integrations. |

### Recommendation: axum 0.8.9

Rationale:
1. A WS-first design is natural: the handler holds an `AppState`, and `on_upgrade` gives an owned task per socket that acts as the single writer for that agent session.
2. tower middleware composes the same way for REST and WS upgrade routes.
3. The best compatibility with the rest of the chosen stack: utoipa-axum, tower-sessions, tower_governor, memory-serve and axum-test all target axum 0.8 [V-poc].
4. Maintained by the tokio team.

Practices:
- Apply cross-cutting layers last, with `Router::layer` after all routes and the fallback.
- Apply the CSP header layer only to the app origin, not to the usercontent router.
- Keep the WS route outside the REST `TimeoutLayer`. The upgrade response returns immediately anyway [I], but scoping it separately avoids surprises.
- Bind with `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` so rate limiting can see peer IPs.
- TLS is terminated at the reverse proxy per §6 of the spec. If IT ever wants in-process TLS, `axum-server` 0.8.0 with rustls is available.

---

## 4. Database access

### Comparison

| | sqlx 0.9 | diesel 2.3 + diesel-async 0.9 | sea-orm 2.0 |
|---|---|---|---|
| Query style | Raw SQL; `query!`/`query_as!` checked against a real DB at compile time | Rust DSL, schema.rs; strong compile-time checks without a DB | ORM (ActiveModel) over sea-query |
| Async | Native | Via diesel-async (separate crate, separate pool `deadpool`/`bb8`) | Native (on sqlx) |
| Postgres-specific SQL (UNNEST arrays, `ON CONFLICT`, `COPY`, `FOR UPDATE SKIP LOCKED`, advisory locks, LISTEN/NOTIFY, partitions) | First-class; `PgListener`, `PgAdvisoryLock`, `copy_in_raw` built in [V-poc] | DSL covers much of it; COPY/LISTEN need raw tokio-postgres | Possible via raw SQL; abstractions get in the way |
| Migrations | `sqlx::migrate!` embeds SQL files; `sqlx-cli` for authoring; migrator takes an advisory lock by default | `diesel_migrations` | `sea-orm-migration` (Rust code) |
| CI story | `cargo sqlx prepare --workspace` → commit `.sqlx/`; build with `SQLX_OFFLINE=true`; `cargo sqlx prepare --check` in CI [V-src] | No DB needed at build | No DB needed at build |
| Cost | DB needed to (re)generate `.sqlx`; macro-heavy builds | DSL learning curve; type-error verbosity | Two abstraction layers; less control over hot-path SQL |

### Recommendation: sqlx 0.9.0, Postgres only

The ingestion hot path consists of Postgres idioms: idempotent `INSERT … ON CONFLICT DO NOTHING RETURNING`, UNNEST batch inserts, COPY, `SKIP LOCKED`, advisory locks, NOTIFY, and day-partitioned telemetry tables. sqlx lets us write that SQL verbatim and still get compile-time checking.

Notable 0.9 changes [V-src]:
- MSRV 1.94.
- `query()` now takes `impl SqlSafeStr`, so dynamic SQL must be wrapped in `AssertSqlSafe(...)`. This is a useful review speed-bump.
- `PgAdvisoryLockGuard` no longer has a lifetime.
- `sqlx.toml` per-crate config (opt-in feature).
- Nullability inference changed (forced generic plans).

Details:
- **Pool**: the built-in `PgPool` (`PgPoolOptions::max_connections`, `acquire_timeout`, `min_connections`). No deadpool/bb8 needed.
  - Size it as (ingest writers + API readers). Start at ~20–30 and keep it below the Postgres `max_connections` share agreed with IT.
  - Keep **one dedicated non-pooled connection** for `PgListener`, and one held by the leader for the advisory lock. Both need **session** semantics.
- **PgBouncer caveat** [I]: in *transaction pooling* mode, LISTEN and session-level advisory locks do not work reliably. Document "connect directly or use session pooling" for those two connections. Optionally add a separate `FLEET_DATABASE_DIRECT_URL`.
- **TLS to Postgres**: `tls-rustls` → `tls-rustls-ring` → **webpki roots** [V-src], which ignores corporate CAs in the OS store. Use `tls-rustls-ring-native-roots`, and/or support `sslrootcert=` in the URL.
- **Transactions for idempotent ingestion**: `let mut tx = pool.begin().await?; … tx.commit().await?;`, then **ack only after commit returns** (PoC `apply_event`).
  - The idempotency key is `(agent_session_id, sequence)` with a unique index.
  - `INSERT … ON CONFLICT DO NOTHING` tells duplicates from new events in the same statement.
  - State transitions use guarded `UPDATE … WHERE state IN (allowed predecessors)` so stale or out-of-order transitions are no-ops (spec §13).
- **Migrations**: embed with `sqlx::migrate!("./migrations")`.
  - Run on start by default (`FLEET_MIGRATE_ON_START=true`). The sqlx migrator takes a Postgres advisory lock, so concurrent instances are safe [I based on `set_locking`, V-src that the API exists].
  - Also ship `fleet-server migrate [--dry-run]` for IT change control.
  - Use forward-only migrations. Write "expand/contract" migrations so N-1 binaries keep working during rolling upgrades (§11 "Versioned upgrades").
- **LISTEN/NOTIFY**: `PgListener::connect_with(&pool)`, `.listen("fleet_live")`, `.recv()` [V-poc].
  - Payloads must be < 8000 bytes.
  - Delivery happens only after commit, in commit order.
  - Identical payloads within one transaction are folded [V-doc].
  - So NOTIFY `{pipeline_id, version}` (or a small delta), not artifacts or large state.

---

## 5. Live state and fan-out

### 5.1 Shape of the projection

```
Hub
 ├─ pipelines: DashMap<PipelineId, Arc<Mutex<PipelineProjection>>>
 │       PipelineProjection { version: u64, jobs, workers, devices, flow_execs, counters, provenance flags }
 ├─ overview:  DashMap<ProjectId, watch::Sender<Arc<ProjectOverview>>>   // coalesced tiles, ≤2 Hz
 ├─ deltas:    DashMap<ProjectId, broadcast::Sender<Arc<Envelope>>>      // ordered, versioned
 ├─ liveness:  DashMap<AgentSessionId, LivenessCell { last_seen: Instant, state }>  // never hits DB per beat
 └─ metrics:   DashMap<WorkerOrDeviceId, RingBuffer<Sample, 360>>        // last 30 min @5s for live charts
```

**Why not an actor per pipeline?** An actor (a task owning the state plus an mpsc inbox) also works, and it is the natural choice if projections later need async work while holding state. Most of our per-event work is "apply a small delta under a lock for microseconds, then publish". A `Mutex` (std or parking_lot, **never held across `.await`**) is simpler, has fewer moving parts and is easier to test.

The agent socket task is already the single writer per session. Cross-session contention happens only on the shared pipeline projection, and it is short. [I]

**Version numbers.** Each pipeline has a monotonically increasing `version` that is **incremented in Postgres inside the ingestion transaction** (`UPDATE pipelines SET version = version + 1 … RETURNING version`) and copied into the projection after commit. This gives three properties:
- Versions survive restarts. A browser that reconnects after a server restart can't confuse new version 3 with old version 3.
- Any future instance can apply deltas in the same version space.
- The hot row lock is held only for the duration of one short transaction. That is fine at tens of events/s per pipeline. If a giant sharded pipeline ever contends, move the counter to `jobs` and use a vector `(job_id, version)`. [I]

### 5.2 broadcast vs watch

| Channel | Semantics | Use for |
|---|---|---|
| `tokio::sync::broadcast` | Ordered; bounded ring; slow receivers get `RecvError::Lagged(n)` and skip to the oldest retained message [V-poc] | Detail-page deltas (flow started/finished, step changes, state transitions) where order and completeness matter |
| `tokio::sync::watch` | Only the latest value; receivers never lag, they just see the newest | Fleet overview tiles, per-worker "current metrics" gauges, counts. Naturally coalesces 5 s metrics into whatever rate the browser drains |

Rules:
- **Serialize once**: publish `Arc<Bytes>` (pre-serialized JSON), not structs that every subscriber re-serializes.
- **Scope channels per project**, and filter by pipeline in the subscriber task. Per-pipeline channels are an optimization for later. Authorization is checked once at subscribe time (project-level RBAC) and re-checked when the session's roles are refreshed.
- **Slow consumers**: each browser socket has a writer task and a bounded `mpsc` queue (e.g. 64 messages). If `try_send` fails, the client is too slow: send `resync`/close and let it reconnect. On `Lagged(n)` send `{"type":"resync"}` and the client re-snapshots. This never blocks the publisher (PoC `live::browser_session`).
- **Browser rate independent of retention** (spec §5 "Update frequencies"): metric deltas go to the browser through a 1 s coalescing tick per pipeline, not one per sample.

### 5.3 Snapshot + delta reconciliation (spec §6 "Live state")

1. Browser opens `wss://…/api/v1/live` with the session cookie. The server checks the **`Origin` header** to prevent cross-site WebSocket hijacking, then authenticates.
2. Client sends `subscribe {project, pipeline?}`.
3. Server **subscribes to the broadcast first**, then reads the snapshot under the projection lock and sends `snapshot {version: N, …}`.
4. Server forwards deltas with `version > N`. The client applies deltas whose version is `last + 1`, ignores those `≤ last`, and on a gap (`> last + 1`) re-requests a snapshot. The PoC's `apply_and_publish` sends while holding the write lock, so snapshot(N) followed by deltas(>N) has no gaps [V-poc].
5. After any disconnect the client re-subscribes and starts again from step 3. REST `GET /api/v1/pipelines/{id}` also returns `version`, so a REST-first initial render works too.

### 5.4 Restore on restart (spec §13 "Backend restart")

On boot, before readiness turns green:
- Load all non-terminal agent sessions, jobs and pipelines, with their flow executions and cursors, from Postgres into the Hub. Then mark readiness.
- Set every worker to `stale` (with provenance "server restarted"). Do **not** set it to `disconnected` or `interrupted`.
- Liveness deadlines become `max(last_seen_at, boot_at) + grace`. Agents reconnect, resume with their last acknowledged sequence, replay, and receive the authoritative state (spec §5 steps 1–5).
- The metrics ring buffers start empty after a restart. Live charts fall back to the persisted 1-minute rows. Mark this gap in the UI as "missing", per §18 provenance.

### 5.5 Horizontal scaling later, without redesign

Put all fan-out behind a trait from day one:

```rust
#[async_trait] trait EventBus: Send + Sync {
    async fn publish(&self, scope: ProjectId, env: Arc<Envelope>) -> Result<()>;
    fn subscribe(&self, scope: ProjectId) -> broadcast::Receiver<Arc<Envelope>>;
}
```

| Stage | Implementation | Notes |
|---|---|---|
| v1 (1 instance) | `InProcessBus` (DashMap of broadcast senders) | Default |
| v2 (2–N instances, same DB) | `PgNotifyBus`: `pg_notify('fleet_live', '{project,pipeline,version,delta?}')` **inside the ingestion transaction**. Each instance runs one `PgListener` and re-publishes into its local in-process bus. If the delta exceeds ~7 KB, send only ids; receivers fetch the change from the DB | Delivered only on commit, in commit order [V-doc], which matches our "publish after commit" rule. Known cost [I]: NOTIFY takes a global lock at commit, serializing notifying commits across the cluster. Fine at hundreds/s, questionable at many thousands/s. |
| v3 (if ever needed) | `NatsBus` (`async-nats` 0.51.0) or Redis pub/sub (`redis` 1.8.1) | Only with demonstrated need (§11) |

Agents and browsers can then hit any instance. Each instance keeps a projection cache for the pipelines it serves, warmed from the DB and kept current from the bus. The only singleton work is the background sweepers, handled by leader election (§6). Sticky sessions are not required. [I]

---

## 6. Background jobs

### Work inventory

| Job | Cadence | Durability need | Proposed mechanism |
|---|---|---|---|
| Liveness/heartbeat expiry (online→stale→disconnected; job → interrupted/unknown after policy timeout) | every 5 s | none; recomputable from `last_seen_at` | In-memory check for connected sockets, plus a leader-only SQL sweep for sessions with no socket on any instance |
| Metrics checkpoint (5 s ring → 1-min rows) | every 30–60 s | best-effort (spec allows) | Per-instance flusher task; UNNEST/COPY batch |
| Retention: delete old telemetry / events / artifacts metadata | hourly/daily | must eventually complete | Leader-only sweep. **Drop day-partitions** for telemetry instead of `DELETE`. Object deletes go through `jobs` |
| Object deletion in S3 | on demand | retry until success | `jobs` table (SKIP LOCKED) |
| Report finalization / reconciliation after `session.completed` or JUnit upload | per session | at-least-once, idempotent | `jobs` table |
| Downsampling aggregates (1-year stats) | daily | idempotent | Leader-only sweep |
| Session store cleanup | hourly | none | Leader-only sweep |

### Options

- **tokio tasks + `interval`**: zero dependencies and ideal for idempotent sweeps. Set `MissedTickBehavior::Delay` [V-poc] and include each loop in the shutdown `CancellationToken`.
- **Postgres-backed queue (hand-rolled)**:
  ```sql
  UPDATE jobs SET state='running', locked_until=now()+interval '5 min', attempts=attempts+1
  WHERE id = (SELECT id FROM jobs WHERE state='queued' AND run_at<=now()
              ORDER BY run_at FOR UPDATE SKIP LOCKED LIMIT 1)
  RETURNING *;
  ```
  Re-queue rows whose lease (`locked_until`) has expired, use exponential backoff, and add a `dedupe_key` unique index for "finalize session X once". Roughly 150–250 lines, with no new schema owner and full control. [I]
- **apalis 1.0 + apalis-postgres 1.0** (2026-10-12) [V-crates/V-src]: tower-based workers, retries, timeouts, unique jobs, heartbeat/orphan re-enqueue, NOTIFY-triggered storage, cron (`apalis-cron`), and an optional board UI. It uses sqlx 0.9, so it is compatible. It is now 1.0, but that release is three weeks old. It also brings its own schema/migrations, and `apalis-postgres` carries a `license-file` (MIT text) that crates.io reports as "non-standard", so cargo-deny will need an exception [V-src].
- `sqlxmq` 0.6 (sqlx 0.8, so incompatible), `underway` 0.2 (durable workflows, low adoption), `graphile_worker` 0.14 (a Rust port of graphile-worker, active, but its schema follows the Node project), `pgmq` (requires an extension), `fang`/`backie` (low activity).

### Recommendation

v1 has three parts:
1. **Leader-only sweeps** as tokio `interval` loops.
2. **A tiny `jobs` table with SKIP LOCKED** for the two retryable job types: finalization and object deletion.
3. **Per-instance metric flushers.**

Revisit apalis-postgres if job types grow beyond about five, or if a UI for job inspection becomes valuable.

**Leader election with advisory locks** (verified API [V-poc]): `PgAdvisoryLock::new("fleet:singleton-jobs").try_acquire(conn)` returns `Either::Left(guard)` when this instance becomes leader.
- Hold the guard's connection for the lifetime of the leadership, and **run the sweep queries on that same connection**. If the connection dies, the lock and leadership are lost together, so two leaders can't run at once.
- Non-leaders retry every ~10 s.
- Sweeps must stay idempotent anyway, because a leader can stall mid-sweep.
- Per-row jobs don't need a leader: SKIP LOCKED already distributes them.

---

## 7. Object storage

### Comparison

| | object_store 0.14.2 | aws-sdk-s3 1.157 | rust-s3 0.38 | opendal 0.59.5 |
|---|---|---|---|---|
| Owner | Apache Arrow (used by DataFusion, Delta-rs, etc.) | AWS (smithy-generated) | Single maintainer | Apache (incubated → TLP) |
| S3 custom endpoint + path-style | `with_endpoint`, `with_virtual_hosted_style_request(false)`, `with_allow_http` [V-poc] | Yes (`force_path_style`) | Yes | Yes |
| Presigned PUT/GET | `Signer::signed_url` [V-poc] | Any operation presignable | Yes | `presign_read/write` (S3 etc.) |
| Presign with signed headers (checksum, content-type) / extra query | **Yes, `SignedUrlOptions` (new in 0.14)** [V-poc] | Yes | Partial | Partial [I] |
| Presigned multipart parts | Yes: `create_multipart` via `MultipartStore`, then sign `partNumber`+`uploadId` per part [V-poc] | Yes | Yes | [I] |
| Local filesystem backend | `LocalFileSystem` (no signer) [V-src] | No | No | `fs` service |
| In-memory backend for tests | `InMemory` | No | No | `memory` |
| Weight | Moderate (reqwest 0.13, aws-lc-rs) | Heavy (smithy runtime, slow compile, large binary) [I] | Light | Heavy (many services behind features) |

### Recommendation: object_store 0.14.2 behind a Fleet `BlobStore` trait

The browser and the agent never get storage credentials (spec §7, §10). The trait:

```rust
trait BlobStore {
    async fn presign_put(&self, key, sha256, content_type, size, ttl) -> PresignedRequest; // url + required headers
    async fn begin_multipart(&self, key) -> UploadId;
    async fn presign_part(&self, key, upload_id, part_no, ttl) -> PresignedRequest;
    async fn complete_multipart(&self, key, upload_id, parts) -> Result<()>;
    async fn presign_get(&self, key, ttl, disposition: Attachment|Inline, content_type) -> Url;
    async fn head(&self, key) -> Meta; async fn delete(&self, key) -> Result<()>;
}
```

- **S3 mode (MinIO, Garage, SeaweedFS, Ceph RGW, AWS)**: object_store `AmazonS3` with path-style addressing and a custom endpoint.
  - The agent PUTs directly to storage.
  - The server binds `content-type` and (optionally) `x-amz-checksum-sha256` into the signature, so the agent can't upload a different type or content [V-poc].
  - **Compatibility caveat [I]:** support for flexible checksums (`x-amz-checksum-*`) and for signed extra headers varies across MinIO, Garage, SeaweedFS and Ceph versions. Make checksum binding a config flag (`storage.s3.signed_checksum = auto|on|off`). In every mode, verify the SHA-256 **server-side after upload**, asynchronously in a job that streams the object, before marking an artifact `verified`.
  - Run a compatibility matrix in CI with testcontainers (MinIO at minimum; Garage and SeaweedFS images if IT uses them).
- **Local mode (small deployments, tests)**: object_store `LocalFileSystem::new_with_prefix(root)`.
  - There is no presigning, so `presign_put` returns a **Fleet-signed URL to the server itself**: `PUT /api/v1/blobs/upload/{token}`, where `token` is an HMAC over `(key, sha256, size, content_type, exp)`. The server streams the body to disk with a size limit and verifies the checksum.
  - The agent sees the same "PUT to a URL with these headers" contract either way.
- **Downloads**: for authorized users, return a short-lived URL (e.g. 5 min). For S3, add `response-content-disposition=attachment` and `response-content-type` as signed query params so the browser downloads rather than renders. Never let S3 serve HTML inline to the user's browser (see §10.2).
- **Multipart threshold**: object_store defaults to ~5 MiB parts [I]. Use multipart above ~64 MiB (videos, debug bundles) and cap total size per artifact class.
- Keys: `{project}/{pipeline}/{job}/{execution}/{artifact_id}` (spec §7), with artifact_id as UUIDv7. Never derive keys from user-supplied file names.

**When aws-sdk-s3 would be better:** only if we need S3-specific features object_store lacks (object lock, lifecycle configuration, bucket policy management). That is unlikely, since IT manages buckets.

---

## 8. Authentication and authorization

### 8.1 Browser SSO (OIDC)

- **openidconnect 4.1.0** [V-crates, V-poc]: `CoreProviderMetadata::discover_async` → `CoreClient::from_provider_metadata(...).set_redirect_uri(...)` → `authorize_url(AuthorizationCode, CsrfToken::new_random, Nonce::new_random)` + `set_pkce_challenge(PkceCodeChallenge::new_random_sha256())`.
  - On callback: check `state`, exchange the code with the PKCE verifier, and verify the ID token (signature, `iss`, `aud`, `nonce`, expiry) with the crate's verifier.
  - Fetch userinfo if groups aren't in the ID token.
  - **Gotcha [V-poc]:** openidconnect 4.1 (via oauth2 5.0) implements its HTTP client trait for **reqwest 0.12**, not 0.13. Passing a reqwest 0.13 client fails to compile. Use `openidconnect::reqwest::Client`. The tree then contains two reqwest versions (≈ +1–2 MB binary, more compile time) [I].
  - **Gotcha [V-src]:** openidconnect's default `rustls-tls` maps to reqwest 0.12 `rustls-tls-webpki-roots`, which **does not trust corporate CAs**. Add `reqwest012 = { package = "reqwest", version = "0.12", default-features = false, features = ["rustls-tls-native-roots"] }` to unify features, or set `default-features = false` on openidconnect and provide your own HTTP adapter.
  - Use a no-redirect HTTP client for token/discovery calls (SSRF hygiene, as in the crate docs) [V-poc].
- **axum-oidc** (0.6.0, 2025-01, ~6k recent downloads, 1.0 in development) is an all-in-one middleware. It is not worth the risk compared with ~200 lines of our own handlers.
- **Sessions**: `tower-sessions` 0.15 `SessionManagerLayer`.
  - Cookie settings: `fleet_sid`, `Secure`, `HttpOnly`, `SameSite=Lax`, inactivity expiry ~8 h [V-poc]. Cycle the session ID on login.
  - **Store: write our own ~60-line `SessionStore` on sqlx 0.9** (a PoC `PgSessionStore` compiles) [V-poc]. The published `tower-sessions-sqlx-store` 0.15.0 depends on `sqlx ^0.8` and `tower-sessions-core ^0.14`, which don't match tower-sessions 0.15 [V-crates]. The repo's `main` has already moved to sqlx 0.9 / core 0.15 but is unpublished [V-src]. A git dependency is an option, but owning 60 lines is cheaper.
  - Expired rows are deleted by the leader sweep.
  - Store only `user_id`, a role snapshot, the CSRF secret and `refreshed_at` in the session.
- **CSRF**:
  - `SameSite=Lax` cookies.
  - All state-changing endpoints are JSON-only (reject form content types).
  - On unsafe methods, require `Origin` (or `Sec-Fetch-Site: same-origin`) to equal the configured public origin. A synchronizer token in a header (`X-Fleet-CSRF`) from the session is optional defense in depth.
  - The OIDC `state` parameter protects the login flow itself.
  - **WebSocket upgrades must check `Origin`**, because browsers send cookies on cross-site WS handshakes.
- **Role and group mapping** to Viewer/Maintainer/Admin per project:
  - Configure the mapping in config/DB, e.g. `{ claim: "groups", value: "mobile-qa", project: "mobile-app", role: "maintainer" }`, plus a global `admin` mapping.
  - Resolve it at login and store the snapshot in the session. Re-resolve when `refreshed_at` is older than ~15 min, or force re-login, so revoked group membership takes effect.
  - Allow local DB role bindings (Admin UI, spec Screen G) on top of IdP groups. Effective role = max(IdP mapping, explicit binding).
  - **Caveat [I]:** some IdPs (notably Entra ID) omit groups past a size limit ("groups overage") or emit group object IDs instead of names. Support matching on IDs and a configurable claim name.

### 8.2 Agent auth: GitHub Actions OIDC → Fleet session token

Verified facts [V-doc]:
- Issuer `https://token.actions.githubusercontent.com`.
- Discovery at `/.well-known/openid-configuration`.
- Claims include `repository`, `repository_id`, `repository_owner`, `repository_owner_id`, `ref`, `ref_type`, `job_workflow_ref`, `job_workflow_sha`, `workflow_ref`, `run_id`, `run_number`, `run_attempt`, `event_name`, `environment`, `actor_id`, `runner_environment`, `repo_property_*`.
- The audience is customizable by the requester (`core.getIDToken(aud)` or `&audience=` on `ACTIONS_ID_TOKEN_REQUEST_URL`).
- GitHub recommends IDs over names. For repositories created after 2026-07-15 (or opted in), `sub` uses an immutable format `repo:OWNER@OWNER-ID/REPO@REPO-ID:…`.
- Dependabot tokens carry `event_name = dynamic`.

Flow:
1. The agent requests an ID token with `audience = <configured Fleet audience>` (e.g. `https://fleet.company.example`) and calls `POST /api/v1/agent/token { id_token, project, ci_context }`.
2. The server verifies the token (PoC `auth::JwksCache::verify`, [V-poc] compile):
   - `decode_header` → `kid`. Look it up in a cached `JwkSet` (JWKS URL from discovery). On an unknown `kid`, **refetch once (single-flight, rate-limited to ~1/min)** to handle key rotation. The baseline TTL is ~10–60 min.
   - `Validation::new(RS256)`: allow only RS256, `set_issuer`, `set_audience`, 30–60 s leeway, and require `exp`/`nbf`/`iat`.
   - jsonwebtoken 11 **requires** choosing `aws_lc_rs` or `rust_crypto` [V-src]. Pick `rust_crypto` if musl cross-builds of aws-lc are painful, otherwise `aws_lc_rs`, which is already in the tree via reqwest 0.13/object_store.
3. **Trust policy per project** (DB-managed, Admin-only):
   - Required: `repository_id` and `repository_owner_id`.
   - Optional allowlists: `ref` globs (e.g. `refs/heads/main`, `refs/pull/*`), `event_name` (default deny `pull_request_target`, `dynamic`), `job_workflow_ref` (pin a reusable workflow), `environment`, `runner_environment`.
   - **Never parse `sub`** (its format differs by repository age and customization).
   - Optionally record `jti` for single use [I].
4. Issue a **Fleet agent token**: an opaque 256-bit random value. Store `sha256(token)` with `{project_id, agent_session_id?, scopes: [ingest, upload], exp: ~2 h or job timeout, claims snapshot}`. Opaque + DB lookup is simpler than minting our own JWTs, revocable, and a lookup per WS connect/upload is cheap. Cache verified tokens in `moka` for 60 s if needed.
   - Scope: one project, and once `session.started` has been accepted, **one agent session**. The token cannot read history, call admin APIs or touch other projects (spec §10).
5. On WS reconnect the agent can present the same Fleet token while it is valid, or get a fresh ID token, which it can request at any point during the job.

**Fork PRs [I, unverified with GitHub docs]:** workflows triggered by `pull_request` from forks generally don't get `id-token: write`, so the agent must treat "no credentials" as "monitoring disabled, test still runs" (spec §13 "Backend unavailable"). Don't advise `pull_request_target` (spec §10 "untrusted PR code").

**Fallback: scoped API keys** (generic CI):
- Format `mfk_<8-char key id>_<43-char base62 secret>`. The prefix makes keys greppable and registrable with secret scanners, and the key id makes lookup O(1).
- Store `sha256(secret)` and compare with `subtle::ConstantTimeEq`.
- **Argon2 is unnecessary** for high-entropy random secrets (argon2 protects low-entropy passwords from brute force), and it would add ~50 ms CPU per WS connect [I].
- Each key has a project, scopes, expiry, `last_used_at` and creator, and rotation is audited (spec §10 Audit).
- Keys are exchanged for the same short-lived Fleet token as OIDC, so downstream code has one auth path.

### 8.3 Authorization model

```rust
enum Principal { User { id, global_admin: bool, roles: HashMap<ProjectId, Role> },
                 Agent { project: ProjectId, session: Option<AgentSessionId>, scopes: Scopes } }
#[derive(PartialOrd, Ord)] enum Role { Viewer, Maintainer, Admin }
enum Action { ReadRuns, ReadArtifacts, DownloadArtifact, ManageProject, ManageCredentials,
              ManageRetention, Ingest, UploadArtifact }
fn authorize(p: &Principal, a: Action, project: ProjectId) -> Result<(), Forbidden>  // exhaustive match
```

- Wire it as an axum extractor (`RequireRole<Maintainer>`), or call it explicitly in handlers that have the project id after loading the resource. Avoid route-level-only checks, because resources are addressed by ID.
- Every list query is filtered by `project_id = ANY($allowed)`. Write a test for each endpoint that a Viewer of project A gets 404/403 on project B.
- **casbin-rs** 2.20: model/policy files and adapters. Too much machinery for three roles.
- **oso**: the legacy OSS library is deprecated in favor of Oso Cloud (last crate release 2024-01) [V-doc].
- **cedar-policy** 4.14 (released yesterday, very active): consider it only if requirements grow to ABAC, e.g. "maintainers may delete artifacts only on non-protected branches".

---

## 9. API contracts and shared types

### 9.1 OpenAPI

- **utoipa 6.0.0 + utoipa-axum 0.3.0** [V-poc]: `OpenApiRouter::with_openapi(ApiDoc::openapi()).routes(routes!(handler))`, then `.split_for_parts()` → `(Router, OpenApi)`. Routes and docs can't drift because the router *is* the spec source. utoipa 6 adds OpenAPI 3.2 [V-src].
- UI: `utoipa-scalar` 0.4 (`Scalar::with_url` needs the `Servable` trait in scope; this is a PoC finding) or `utoipa-swagger-ui` 10. Serve it at `/api/docs` (authenticated), and the JSON at `/api/v1/openapi.json`.
- **aide** 0.15.1 (0.16 alpha) is more axum-native but slower-moving. **poem-openapi** requires poem.

### 9.2 WebSocket message schema

- WS messages are a tagged enum: `#[serde(tag = "type")]`, with `#[serde(rename = "flow.started")]` etc.
- Derive `schemars::JsonSchema` and publish `/api/v1/schema/agent-protocol.v1.json` [V-poc].
- Generate a golden schema file in CI and fail on unreviewed diffs. That file is the compatibility contract between agent and server versions.
- Forward compatibility [I]: ignore unknown fields (don't use `deny_unknown_fields` on payloads), and **nack unknown event types with a permanent error code** rather than closing the socket.

### 9.3 Validation

serde parsing gives type safety. Add **garde** for semantic limits (string lengths, ranges, collection sizes) [V-poc]. Use **nutype** newtypes for domain identifiers (`ProjectSlug`, `FlowPath` normalized repository-relative, `Sha256Hex`, `ArtifactKind`) so invalid values can't be constructed. `validator` 0.21 is the alternative to garde.

For redaction and log sanitization (spec §10), add a `fleet-sanitize` module applied at ingestion, before persistence and broadcast.

### 9.4 Sharing types: `fleet-protocol` crate

- `fleet-protocol` depends only on `serde`, `uuid`, `time`.
- Optional features: `schema` (schemars), `openapi` (utoipa), `ts` (ts-rs), `validate` (garde).
- The agent enables none of these, keeping it lean (spec §4 overhead). The server enables all of them.
- It stays `wasm32-unknown-unknown`-compatible: no tokio, no I/O. A Rust/WASM frontend (Leptos/Dioxus) can use the types directly. This was not built in the PoC, but the wasm target is installed and the crate is dependency-light [I].

### 9.5 TypeScript (if the frontend is TS)

- **ts-rs 12** [V-poc]: `#[derive(TS)] #[ts(export)]`, and `cargo test` writes `bindings/*.ts`. Run it in CI and `git diff --exit-code` on the bindings.
- An alternative for REST only is `openapi-typescript` on the generated OpenAPI. Using ts-rs for everything gives one mechanism for both REST DTOs and WS messages.
- `specta` 2 is still RC. `typeshare` 1.0.5 (1Password) is CLI/annotation-based and aimed at multi-language mobile, which is unnecessary here.

### 9.6 TLS roots summary (spec §10 "Corporate DNS/certificates")

| Component | Default roots | Required setting |
|---|---|---|
| reqwest 0.13 (`rustls`) | rustls-platform-verifier, OS store [V-src] | OK. Ensure the image has `/etc/ssl/certs` with the corporate CA, or set `SSL_CERT_FILE` [I] |
| openidconnect → reqwest 0.12 `rustls-tls` | **webpki-roots only** [V-src] | Add the reqwest 0.12 `rustls-tls-native-roots` feature |
| sqlx `tls-rustls` | **webpki-roots** [V-src] | Use `tls-rustls-ring-native-roots`, or `sslrootcert=` |
| object_store `aws` (reqwest 0.13 `rustls`) | platform verifier | OK; also `ClientOptions` root-cert options [I] |

---

## 10. Serving the frontend and untrusted HTML

### 10.1 The SPA

- **memory-serve 2.4** [V-src]:
  - Assets embedded at build time via `build.rs` and brotli-compressed at compile time.
  - Gzip/brotli negotiation, ETag/If-None-Match, cache-busted routes and SPA fallback routing.
  - Assets are served dynamically from disk in debug builds, which is convenient with `vite build --watch`.
  - Its user base is small (~31k recent downloads). If that is a concern, **rust-embed 8.13** with a ~30-line handler (PoC `spa()`) is the mainstream choice. `tower-http::services::ServeDir` + `ServeFile` fallback also works if the image ships a `/web` directory instead of embedding.
- Routing order: `/api/*` and `/ws/*` first, with **`/api/*` 404 returned as JSON, never `index.html`**. Then static assets, then the fallback to `index.html` for client routes.
- Cache headers:
  - `index.html` gets `Cache-Control: no-cache` (revalidate with ETag).
  - Hashed assets (`/assets/app.3f9c.js`) get `public, max-age=31536000, immutable`.
- Security headers on the app origin:
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: <usercontent-origin> <s3-origin>; connect-src 'self' wss://<fleet-host>; frame-src <usercontent-origin>; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`
  - `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, `Strict-Transport-Security` (or set at the proxy).
  - Use `SetResponseHeaderLayer::if_not_present` [V-poc].

### 10.2 Untrusted Maestro HTML reports (spec §10 last bullet)

Threat: a report HTML (or any artifact a PR can influence, such as a screenshot name or log text) contains script. If served from the app origin, that script could read the API with the user's cookie.

What others do:
- GitLab long ago concluded that user HTML artifacts must not load on the main GitLab domain. Its online HTML artifact viewing goes through **GitLab Pages on a separate domain** and opens in a new tab [V-doc, older issue threads].
- Google's guidance [V-doc] uses **sandbox domains** for active content. For inactive files it recommends `Content-Disposition: attachment` + `nosniff` + `CSP: sandbox; default-src 'none'` + `Cross-Origin-Resource-Policy`. For HTML that must render, it recommends `Content-Security-Policy: sandbox` on a separate origin.
- MDN [V-doc]: a `sandbox` CSP without `allow-same-origin` gives the document an opaque (`null`) origin. `sandbox` is ignored in `<meta>` and in Report-Only.
- Grafana [I]: sanitizes user HTML in text panels by default (`disable_sanitize_html=false`). That suits snippets, not whole report documents.

Recommendation, in increasing strictness:
1. **Default: download only.** HTML reports are served with `Content-Disposition: attachment; filename="report.html"`, `Content-Type: text/html`, `X-Content-Type-Options: nosniff` and `CSP: sandbox; default-src 'none'`. Users open them locally.
2. **"View in browser" (opt-in by IT config, requires DNS/TLS)**: a separate hostname, e.g. `fleet-usercontent.company.example`, **not a subdomain sharing cookies with the app** (the app cookie is host-only, with no `Domain=` attribute).
   - It is served by the same binary on the same port, chosen by `Host` header via a different router with no session layer and no CORS.
   - Each view URL is a short-lived HMAC-signed capability (`/r/{token}/index.html`, TTL 10 min, bound to user + artifact) because there are no cookies on that origin.
   - Response headers: `Content-Security-Policy: sandbox allow-scripts allow-popups; default-src 'none'; script-src 'unsafe-inline' 'self'; style-src 'unsafe-inline' 'self'; img-src 'self' data:; connect-src 'none'; frame-ancestors <app-origin>`, plus `nosniff`, `Cross-Origin-Resource-Policy: same-origin` and `Referrer-Policy: no-referrer`.
   - `allow-scripts` without `allow-same-origin` lets the report's own JS run in an opaque origin with no network access. Whether Maestro's HTML report needs JS at all is **unverified**; drop `allow-scripts` if it doesn't.
   - The app embeds it in `<iframe sandbox="allow-scripts" src="https://usercontent/...">` or opens a new tab.
3. **Never** let S3/MinIO serve HTML inline to users. You can't set CSP on presigned GETs, and the storage origin may host other artifacts. Force `response-content-disposition=attachment` on presigned GETs for HTML/SVG/XML. Re-encode or type-check images (screenshots are PNG/JPEG only; **SVG is not in the allowlist**). [I]
4. Logs are rendered as text in the app, never as HTML (ANSI → spans in the frontend with escaping).

Adding the usercontent hostname to the deployment is a §11 change (one more DNS name + certificate). If IT can't provide it, fall back to option 1 only.

---

## 11. Operational concerns

### Configuration

- **figment** [V-poc]: `Serialized::defaults` → `Toml::file("fleet.toml")` → `Env::prefixed("FLEET_").split("__")`. For example, `FLEET_DATABASE__URL` and `FLEET_STORAGE__S3__ENDPOINT`. Errors name the source and key, which helps IT debug.
- figment's last release was 2024-05 [V-crates]. It is feature-complete, but if that bothers you, `config` 0.15.27 is equivalent.
- Add `*_FILE` indirection for Docker/K8s secrets (`FLEET_DATABASE__PASSWORD_FILE=/run/secrets/db`). This is a small custom provider [I].
- Wrap secrets in `secrecy::SecretString` so they never show up in `Debug` logs.
- Add `fleet-server check-config`, which prints the effective config with secrets redacted.

### Graceful shutdown

- `axum::serve(...).with_graceful_shutdown(signal)` stops accepting connections and waits for in-flight HTTP requests [V-poc API].
- **Upgraded WebSocket connections are detached from hyper's connection tracking [I]**, so the server must drain them itself:
  1. On SIGTERM: set `draining=true`. `/readyz` returns 503 so the load balancer stops sending traffic.
  2. `CancellationToken::cancel()`. Every agent socket task finishes its in-flight transaction, sends any pending ack, sends `{"type":"server.draining","reconnect_after_ms": random 1000–5000}`, and then sends Close **1012 (Service Restart)** [V-poc]. Browser sockets get 1012 and reconnect with jitter.
  3. Await all socket tasks via `tokio_util::task::TaskTracker` with a drain deadline of about 15 s. Docker's default stop timeout is 10 s, so set `stop_grace_period: 30s` in Compose (and `terminationGracePeriodSeconds: 30` in K8s later) so the drain can finish before SIGKILL.
  4. Run a final metrics checkpoint flush, then `pool.close().await`.
- Agents treat 1012/draining as "reconnect with backoff, keep buffering" (spec §13).

### Health

- `GET /livez`: process up (no dependencies).
- `GET /readyz`: DB `SELECT 1` within 1 s; migrations at the expected version; projections restored; not draining.
- Object storage is checked *non-fatally* and reported in `/api/v1/health` (spec §6) as a degraded component. Artifact uploads failing shouldn't take ingestion down.

### Logs, traces, metrics

- `tracing_subscriber::fmt().json()` with `EnvFilter` [V-poc]. Use `TraceLayer` with a request-id (`tower-http` `request-id` feature) and propagate it to log spans. Never log tokens; redact `Authorization` and `Cookie` headers.
- Optional cargo feature `otel`: `tracing-opentelemetry` 0.34 + `opentelemetry-otlp` 0.33, configured by the standard `OTEL_EXPORTER_OTLP_ENDPOINT`. These crates are still 0.x and churn, so isolate them in `fleet-server/src/telemetry.rs`.
- **Prometheus**: `metrics` + `metrics-exporter-prometheus` (`install_recorder()` + a `/metrics` handler [V-poc]), on a separate internal port or behind a bearer token. Key series:
  - `fleet_ws_connections{kind=agent|browser}`
  - `fleet_events_ingested_total{type}`
  - `fleet_ingest_tx_seconds` (histogram)
  - `fleet_ingest_batch_size`
  - `fleet_ack_lag_events`
  - `fleet_broadcast_lagged_total`
  - `fleet_browser_slow_disconnects_total`
  - `fleet_db_pool_in_use`
  - `fleet_jobs_queued{kind}`
  - `fleet_leader{instance}`
  - `axum-prometheus` for HTTP RED metrics.

### Limits and timeouts

| Limit | Setting |
|---|---|
| REST JSON body | `DefaultBodyLimit`/`RequestBodyLimitLayer` 1 MiB [V-poc] |
| Local-mode blob upload | per-route limit by artifact class (e.g. 50 MiB screenshots/logs, 500 MiB video) |
| **WS max message / frame** | **256 KiB / 64 KiB** for agents, 16 KiB for browser → server. **tungstenite defaults are 64 MiB / 16 MiB** [V-src], so they must be set |
| WS buffers | `read_buffer_size`/`write_buffer_size` default to 128 KiB each [V-src]. Consider 16–32 KiB for thousands of mostly-idle browser sockets [I]. Set `max_write_buffer_size` for backpressure |
| WS keepalive | server Ping every 20 s; close if no Pong/frame within 60 s. Common proxy idle timeouts are ~60 s (nginx `proxy_read_timeout`) [I], so keepalive must stay below that |
| REST timeout | `TimeoutLayer::with_status_code(408, 30s)` [V-poc]; header read timeout (axum 0.9 applies hyper's by default) |
| Rate limits | `tower_governor` on `/api/v1/agent/token` and login (per IP), and on REST (per session). **Behind a reverse proxy**, per-IP keys need `SmartIpKeyExtractor`, which trusts `X-Forwarded-For`/`Forwarded`. Only enable it when the proxy overwrites these headers [I]. For agents, key on the token id |
| Concurrency caps | in-app: max agent sockets per project/token (DashMap counter), max browser sockets per user; `ListenerExt::limit_connections` arrives in axum 0.9 [V-src] |
| Per-session event rate | token bucket per agent socket (e.g. 200 events/s sustained, burst 2000); excess → `nack{retry_after}` |

---

## 12. Testing

| Layer | Tooling | Notes |
|---|---|---|
| Pure logic (state machines, reconciliation, redaction, classification) | plain `#[test]`, `proptest` [I] for transition invariants ("no sequence of events can produce `passed` without evidence `final`") | Most correctness lives here; keep it I/O-free in `fleet-core` |
| DB integration | **`#[sqlx::test(migrations = "./migrations")]`**: each test gets a fresh database with migrations applied [V-poc compile; runtime needs `DATABASE_URL`] | Run against a Postgres service container in CI |
| Containers | **testcontainers** 0.27 + **testcontainers-modules** 0.15 (`postgres`, `minio`). Version skew: modules require testcontainers ^0.27 while the latest is 0.28 [V-crates] | Use for MinIO presign round-trips and S3 compatibility; Garage/SeaweedFS via `GenericImage` |
| HTTP/WS handlers | **axum-test** 21.1 (`TestServer`, `ws` feature) [V-poc], or `tower::ServiceExt::oneshot` for single requests | A real WS test with axum + tokio-tungstenite passed in the PoC |
| Resilience (spec §15) | A test harness that runs the real server in-process, a fake agent client from `fleet-protocol`, and fault injection (kill socket mid-batch, restart server, replay duplicates, reorder, clock skew) | Gate for Phase 2: "concurrent agents can publish without corrupting run state" |
| Load: WS ingestion | **A custom Rust load generator** (`fleet-loadgen`) reusing the agent's protocol client: N virtual agents × M devices, 5 s metrics, bursty lifecycle with realistic flow/step timings, reconnect storms, replays | Best fidelity (same seq/ack semantics) |
| Load: alternatives | k6 (`k6/websockets` module) [I], goose 0.19.1 (HTTP-oriented; WS support not first-class [I]) | k6 for REST/dashboard read load |
| Test runner | `cargo-nextest` | Parallel, per-test timeouts |

---

## 13. Container build

```dockerfile
# syntax=docker/dockerfile:1.7
FROM rust:1.97-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p fleet-server
COPY . .
ENV SQLX_OFFLINE=true
# web assets built in a separate node/wasm stage and copied to crates/fleet-web-assets/dist first
RUN cargo build --release -p fleet-server

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/fleet-server /fleet-server
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/fleet-server", "serve"]
```

- **cargo-chef** caches the dependency build layer. Without it, every source change rebuilds ~490 crates. A cold release build of the PoC stack took **~11 min on 4 vCPU** with thin LTO [V-poc].
- **SQLX_OFFLINE=true** + committed `.sqlx/` is mandatory, because the Docker build has no database.
- **Base image**: `distroless/cc-debian12` (glibc + libgcc + ca-certificates, ~20–25 MB [I]) plus a 13.4 MB binary [V-poc], so **~35–40 MB** total.
  - Static musl + `scratch`/`distroless/static` saves ~15 MB, but musl's allocator is slow under multi-threaded load [I], so you would need `mimalloc` as the global allocator.
  - aws-lc-rs (pulled by reqwest 0.13 / object_store / optionally jsonwebtoken) needs a C toolchain, and sometimes cmake/clang, for musl cross builds [I].
  - Recommendation: **glibc distroless for v1**.
- **Multi-arch (amd64 + arm64)**:
  - Use native arm64 runners (GitHub `ubuntu-24.04-arm`) and `docker buildx imagetools create` to stitch a manifest.
  - Or use `cargo-zigbuild` 0.24 to cross-compile from amd64 and `COPY` the per-arch binary into per-platform images.
  - **Avoid QEMU-emulated `cargo build`**, which is many times slower [I].
- Supply chain: run `cargo deny check` (licenses, advisories, duplicate versions) and `cargo auditable build` (embeds the dependency list for SBOM scanners).
- Pin `rust-toolchain.toml` to 1.97 (sqlx 0.9 needs ≥1.94 [V-src]; loco needs 1.94).

---

## 14. Performance sanity

### Workload model (assumptions [I])

The spec gives no numbers, so this models a generous internal deployment: **200 concurrent CI jobs** with **4 emulators each** (800 devices, 200 workers, 200 agent sockets).

| Stream | Rate per unit | Total |
|---|---|---|
| Worker + device metrics (5 s) | 0.2 msg/s × 1000 sources | **~200 msg/s** |
| Lifecycle: flow ~60 s with ~20 steps → ~42 events/flow | ~0.7 ev/s per device | **~560 ev/s steady** |
| Bursts (suite start, retries, reconciliation replay after reconnect storm) | 5–10× | **~3–6k ev/s for seconds** |
| Browser subscribers | 50–200 sockets | deltas fanned out ×subscribers, but coalesced |

### Server-side CPU and memory [I]

- serde_json parse + garde validation of a ~0.5–2 KB message costs on the order of microseconds. Even 6k msg/s is a few percent of one core.
- tokio comfortably holds thousands of idle sockets. At tungstenite's default 128 KiB read + 128 KiB write buffers, worst-case buffer memory is ~256 KiB/socket (~250 MB at 1,000 sockets). Shrinking the buffers to 32 KiB brings that to ~64 MB.
- Broadcast fan-out of pre-serialized `Arc<Bytes>` is cheap.
- **One axum instance on 2 vCPU / 1–2 GB should handle 1,000+ agent sockets and several thousand browser sockets.** This needs confirming with `fleet-loadgen`.

### The bottleneck: Postgres commits

- A commit needs a WAL fsync. On typical network/SSD storage, a single connection does ~1–3k commits/s, and the server as a whole maybe 5–20k commits/s with group commit [I]. One transaction per event at 6k ev/s bursts is therefore at the edge for a modest IT-managed Postgres.
- Mitigations, all compatible with the spec's "ack durable ingestion":
  1. **Micro-batching per socket**: the socket task drains up to 100 already-received frames, or waits 10–25 ms, then applies them in **one transaction** and acks the highest contiguous sequence. Commit rate per agent drops to ≤40–100/s at burst and ~1/s steady, and ack latency stays under 50 ms. The agent already supports cumulative acks via "last acknowledged sequence" in §5.
  2. **Heartbeats never touch the DB**: they update in-memory `LivenessCell`s, and `last_seen_at` is persisted lazily (every 30 s, batched for all sessions in one UPDATE … FROM UNNEST).
  3. **Metrics are not individually durable**: the latest values feed live views, and a per-instance flusher writes 1-minute aggregates (min/avg/max/last) every 30–60 s with `INSERT … SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], …)` [V-poc compile]. For large flushes use `COPY … FROM STDIN` (`copy_in_raw` [V-poc compile]). Timescale benchmarks find UNNEST ~50% faster than multi-row VALUES at batch 1000, it avoids the 65,535 bind-parameter limit, and COPY is fastest of all [V-doc]. If raw 5 s samples must be kept for active runs, put them in a **day-partitioned** table dropped after N days.
  4. **Step events**: store them compactly (one row per step transition, payload JSONB small). Consider coalescing `step.started`+`step.completed` into one row updated in place. The protocol/DB researcher decides this.
  5. Indexes: keep the hot ingest tables lean (PK + `(session_id, sequence)` unique + FK indexes). Put analytics on separate aggregate tables built by jobs.
- With micro-batching, a burst of 6k events/s from 200 sockets becomes ≤ ~2k small transactions/s, and steady state becomes a few hundred/s. That is comfortable for one Postgres 16 on 2–4 vCPU [I]. **This must be validated in Phase 2 with `fleet-loadgen` + `pg_stat_statements`.** Treat these numbers as planning estimates, not measurements: no live DB was available in this environment.

---

## 15. Proposed module and crate layout

```
maestro-fleet/
├─ Cargo.toml                    # [workspace], shared [workspace.dependencies], rust-version = "1.97"
├─ rust-toolchain.toml
├─ deny.toml                     # cargo-deny: licenses (+ apalis exception if adopted), advisories, bans
├─ .sqlx/                        # sqlx offline query metadata (committed)
├─ migrations/                   # 0001_init.sql … forward-only, expand/contract
├─ crates/
│  ├─ fleet-protocol/            # wire types only: envelopes, events, acks, REST DTOs, ids, error codes
│  │                             # features: schema | openapi | ts | validate ; no tokio; wasm-safe
│  ├─ fleet-core/                # pure domain: state machines (job/flow/worker liveness), reconciliation,
│  │                             # provenance, redaction/sanitize, policy (Role/Action/authorize) — no I/O
│  ├─ fleet-store/               # sqlx repositories, ingestion tx (apply_batch), queries for REST, jobs table,
│  │                             # session store, partition mgmt; owns migrations! macro
│  ├─ fleet-blob/                # BlobStore trait + object_store S3/Local impls + fleet-signed upload tokens
│  ├─ fleet-server/              # the binary
│  │  └─ src/
│  │     ├─ main.rs              # clap: serve | migrate | check-config | create-admin
│  │     ├─ config.rs            # figment, secrets, *_FILE
│  │     ├─ app.rs               # Router assembly, layers, usercontent host router
│  │     ├─ state.rs             # AppState { pool, hub, bus, blob, auth, cfg, shutdown, tracker }
│  │     ├─ telemetry.rs         # tracing, otel (feature), prometheus
│  │     ├─ auth/                # oidc.rs, session_store.rs, gha_oidc.rs (JWKS cache), api_keys.rs,
│  │     │                       # agent_tokens.rs, extractors.rs (CurrentUser, AgentPrincipal, RequireRole)
│  │     ├─ ingest/              # agent_ws.rs (socket task), batcher.rs, ack.rs, limits.rs
│  │     ├─ live/                # hub.rs (projections), bus.rs (EventBus: InProcess | PgNotify), browser_ws.rs
│  │     ├─ api/                 # projects.rs pipelines.rs jobs.rs workers.rs devices.rs executions.rs
│  │     │                       # flows.rs artifacts.rs analytics.rs health.rs admin.rs openapi.rs
│  │     ├─ artifacts/           # upload brokering, verification job, download links, usercontent serving
│  │     ├─ jobs/                # leader.rs (advisory lock), sweeps.rs (liveness, retention, partitions,
│  │     │                       # sessions), queue.rs (SKIP LOCKED worker), finalize.rs, metrics_flush.rs
│  │     └─ web.rs               # embedded SPA (memory-serve / rust-embed)
│  ├─ fleet-web-assets/          # build.rs embedding dist/ (keeps the server crate's rebuilds fast)
│  ├─ fleet-agent/               # (other research area) depends on fleet-protocol only
│  └─ fleet-loadgen/             # load generator using fleet-protocol + the agent's client module
└─ web/                          # frontend (TS or Rust/WASM)
```

Why split the crates:
- `fleet-core` stays fast to test and free of I/O.
- `fleet-protocol` stays tiny for the agent.
- `fleet-store` isolates sqlx macro compile cost and `.sqlx` regeneration.
- `fleet-web-assets` stops frontend changes from invalidating server compilation.

---

## 16. Ingestion path (internal architecture)

```
 CI runner (agent)                                    Fleet Server (one process)
 ─────────────────                                    ───────────────────────────────────────────────────────────────
                    wss /api/v1/agent/stream
 [bounded queue] ── Authorization: Bearer <fleet token> ──►  axum route ─► auth extractor (token → AgentPrincipal{project, session})
   seq 1..n                                                    │            Origin n/a (non-browser); per-project socket cap
                                                               ▼
                                                     WebSocketUpgrade (max_msg 256KiB, max_frame 64KiB, small buffers)
                                                               │ on_upgrade → spawn socket task (tracked by TaskTracker)
                                                               ▼
 ┌──────────────────────────────── per-agent socket task (single writer for this agent session) ───────────────────────────┐
 │                                                                                                                          │
 │  1. READ frame ──► serde decode Envelope (tagged enum, schema_version) ──► garde/nutype validate ──► sanitize/redact     │
 │        │ invalid: nack{seq, code:permanent}            unknown type: nack permanent (don't drop the socket)            │
 │        ▼                                                                                                                 │
 │  2. SCOPE CHECK: envelope.project/session == principal scope; seq > cursor? (≤ cursor ⇒ duplicate ⇒ re-ack, skip)      │
 │        ▼                                                                                                                 │
 │  3. CLASSIFY ─┬─ heartbeat/keepalive ──► LivenessCell.touch() (memory only) ───────────────────────────────┐            │
 │               ├─ worker.metrics ───────► metrics ring + watch::send (coalesced) ─► "accepted" ack ──────────┤ no DB tx  │
 │               └─ lifecycle (session/suite/flow/step/device/artifact.available) ──┐                         │            │
 │                                                                                    ▼                         │            │
 │  4. MICRO-BATCH: collect ≤100 frames or ≤20ms (socket reads pause meanwhile ⇒ TCP backpressure)            │            │
 │        ▼                                                                                                     │            │
 │  5. ONE TRANSACTION (sqlx, pool.begin)                                                                       │            │
 │     a. INSERT INTO agent_events(session_id, seq, event_id, …) SELECT * FROM UNNEST(…)                        │            │
 │        ON CONFLICT (session_id, seq) DO NOTHING RETURNING seq          ── idempotency                        │            │
 │     b. for each *new* event: fleet-core state machine → guarded UPDATE/INSERT                                 │            │
 │        (UPDATE flow_attempts SET state='failed' … WHERE state IN ('pending','running'))  ── stale transitions │            │
 │                                                                                           become no-ops      │            │
 │     c. UPDATE pipelines SET version = version + k WHERE id=$1 RETURNING version        ── versions persisted │            │
 │     d. UPDATE ingestion_cursors SET last_seq = $max_contiguous                                                │            │
 │     e. (multi-instance only) SELECT pg_notify('fleet_live', '{pipeline, version}')                            │            │
 │     f. COMMIT ───────── fails? ⇒ no ack, nack{retry_after}; agent keeps events & replays                     │            │
 │        ▼                                                                                                     │            │
 │  6. ACK AFTER COMMIT: send {"type":"ack","upto":max_contiguous_seq}  (crash before this ⇒ replay ⇒ dedup)    │            │
 │        ▼                                                                                                     │            │
 │  7. PROJECTION UPDATE: hub.pipelines[pid].lock(): apply deltas, set version from step 5c (no await held)    │            │
 │        ▼                                                                                                     ▼            │
 │  8. BROADCAST: serialize once → Arc<Bytes> ─► EventBus.publish(project) ; overview watch::send(summary)  ◄───┘            │
 └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
                                   │ broadcast::Sender<Arc<Envelope>> (cap 1024, per project)
                 ┌─────────────────┼──────────────────┐
                 ▼                 ▼                  ▼
          browser task A     browser task B      browser task C        each: subscribe → snapshot(v=N) → deltas v>N
          (bounded mpsc 64)  Lagged(n)? ⇒        try_send full ⇒       filter by pipeline + authz scope
                             send resync         close (slow client)   1s coalescing for metric tiles
                                   ▼
                         wss /api/v1/live  ──►  browser re-snapshots on resync / gap / reconnect

 Side paths:
  • Artifact upload: agent POST /api/v1/artifacts (metadata, sha256, size, type∈allowlist) → server inserts row (state=pending)
    → BlobStore.presign_put/multipart → agent PUTs to S3 (or fleet local upload URL) → agent sends artifact.available
    → jobs: verify sha256/size/type → state=verified → delta broadcast.
  • Leader (advisory lock holder): liveness sweep (stale→disconnected→interrupted/unknown with provenance), retention,
    partition drop, session cleanup.  Per instance: metrics checkpoint flusher (UNNEST/COPY every 30–60s).
  • Shutdown: cancel token → each socket task finishes current tx+ack → server.draining + Close 1012 → TaskTracker.wait.
```

Invariants this design enforces:
1. **Nothing is acked or broadcast before it is committed.** Browsers can't see state that a crash would undo.
2. **A duplicate never mutates state.** Only rows newly inserted in step 5a drive transitions.
3. **Stale transitions are rejected in SQL** (guarded updates), not only in memory.
4. **Liveness and metrics never drive per-message commits.**
5. **Each agent session has exactly one writer**, its socket task. If the agent reconnects while the old socket is half-open, the new connection **takes over**: the server cancels the old task via a per-session `CancellationToken` stored in the Hub before accepting the resume.

---

## 17. Risks and trade-offs compared with the original TypeScript/Fastify plan

Where Rust costs **more** effort:

| Topic | Rust cost | Mitigation |
|---|---|---|
| Compile times | Cold release build ~11 min for this stack on 4 vCPU; incremental release ~2.4 min, incremental debug ~11 s [V-poc]. sqlx macros and utoipa derives add to incremental builds | cargo-chef, sccache in CI, crate split (§15), `lto="thin"` only in release, mold/lld linker for dev [I] |
| Ecosystem version skew | Already visible today [V-crates]: tower-sessions-sqlx-store (sqlx 0.8), axum-login (tower-sessions 0.14), openidconnect (reqwest 0.12), testcontainers-modules (testcontainers 0.27), loco (tower-http 0.6). Upcoming axum 0.9 breaking changes | Own small glue (session store ~60 LOC), avoid thin wrapper crates, `cargo deny` duplicate check, scheduled dependency upgrades |
| Auth glue | Node has Passport/openid-client "batteries"; in Rust we write the OIDC callback, session, CSRF, role mapping and JWKS cache ourselves (~600–900 LOC est.) [I] | Concentrated in `auth/`, heavily tested; the primitives (openidconnect, jsonwebtoken) are solid |
| Hiring/onboarding | Fewer engineers fluent in async Rust; borrow-checker friction with shared mutable projections | Keep the concurrency model boring (Mutex + broadcast, no lock across await), document the patterns |
| Frontend type sharing | TS/TS sharing is free; Rust → TS needs codegen (ts-rs) and a CI drift check | Already designed in (§9.5); or a Rust/WASM frontend shares types natively |
| sqlx offline workflow | `.sqlx` must be regenerated against a DB whenever queries change; CI must check it | `cargo sqlx prepare --check` in CI; dev compose provides the DB |
| Prototyping speed | Iterating on the event model is slower than in TS | Fix the protocol early (another researcher's area); use `serde_json::Value` payload escape hatches only behind versioned types |

Where Rust is **better** for this product:
- **One small static binary**: ~13 MB, ~35–40 MB image, no Node runtime or `node_modules` in production. That makes IT's job (spec §11) simpler, and it matches the agent's language, which is presumably also Rust.
- **Shared protocol crate** between agent and server, with compile-time exhaustiveness on event types. Adding a new event type breaks compilation wherever it isn't handled.
- **Predictable latency and memory with thousands of sockets.** There is no GC and no single-threaded event loop to block. Fastify + ws would also be fine at this scale [I], so this is a margin, not a necessity.
- **Compile-time-checked SQL** with sqlx, versus runtime-only checks with most Node query builders.
- Strong typing of state machines in `fleet-core` directly supports the spec's §18 provenance principle.

Honest bottom line: Rust adds roughly **15–30% more initial backend effort** [I]. Most of it goes into auth glue, build/CI setup and dependency wrangling. In return the server is easier to operate and harder to break subtly. The architecture in §5–§16 is language-neutral, and the biggest risks (Postgres write amplification, ack semantics, liveness after restart, untrusted HTML) would exist in TypeScript too.

---

## 18. Open questions to resolve

1. Does Maestro's HTML report need JavaScript? This determines whether `allow-scripts` is needed in the sandbox CSP.
2. Can IT provision a second hostname plus certificate for usercontent? If not, HTML reports are download-only.
3. Is IT's Postgres behind PgBouncer in transaction mode? If so, the listener and leader connections need a direct URL.
4. Which S3 implementation does IT run (MinIO, Ceph RGW, Garage, SeaweedFS)? Run the checksum/signed-header compatibility test against it.
5. Which IdP is used (Entra ID, Okta, Keycloak, Google)? This determines the groups claim format and overage behavior.
6. Expected scale: number of concurrent jobs and devices. This feeds the `fleet-loadgen` profile and the Postgres sizing in §14.
7. The protocol researcher should confirm cumulative acks and the "accepted" vs "durable" ack distinction for metrics (§0 item 4).

---

## Sources

Crate registries and docs (all accessed 2026-11-03):
- crates.io API, per crate: `https://crates.io/api/v1/crates/<name>` and `/<name>/<version>/dependencies` for axum, axum-extra, actix-web, actix-ws, poem, poem-openapi, loco-rs, salvo, tower, tower-http, tokio, hyper, tokio-tungstenite, sqlx, sqlx-cli, diesel, diesel-async, sea-orm, dashmap, papaya, scc, apalis, apalis-postgres, underway, sqlxmq, pgmq, graphile_worker, object_store, aws-sdk-s3, rust-s3, opendal, openidconnect, oauth2, axum-oidc, tower-sessions, tower-sessions-sqlx-store, axum-login, jsonwebtoken, josekit, jwt-simple, argon2, sha2, casbin, oso, cedar-policy, utoipa, utoipa-axum, utoipa-scalar, utoipa-swagger-ui, aide, schemars, garde, validator, nutype, ts-rs, specta, typeshare, rust-embed, memory-serve, figment, config, envy, tracing, tracing-subscriber, opentelemetry, opentelemetry-otlp, tracing-opentelemetry, metrics, metrics-exporter-prometheus, axum-prometheus, tower_governor, testcontainers, testcontainers-modules, axum-test, goose, cargo-chef, cargo-zigbuild, cargo-deny, cargo-auditable, cargo-nextest, mimalloc, reqwest, async-nats, redis.
- axum CHANGELOG (Unreleased / 0.9 changes, 0.8.9): https://raw.githubusercontent.com/tokio-rs/axum/main/axum/CHANGELOG.md
- axum WebSocket extractor docs: https://docs.rs/axum/0.8.9/axum/extract/ws/index.html
- tungstenite WebSocketConfig defaults (source, 0.29.0): https://docs.rs/tungstenite/0.29.0/tungstenite/protocol/struct.WebSocketConfig.html
- sqlx 0.9.0 CHANGELOG: https://raw.githubusercontent.com/launchbadge/sqlx/main/CHANGELOG.md
- sqlx-cli offline mode: https://raw.githubusercontent.com/launchbadge/sqlx/main/sqlx-cli/README.md
- sqlx PgListener / PgAdvisoryLock: https://docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgListener.html , https://docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgAdvisoryLock.html
- PostgreSQL NOTIFY: https://www.postgresql.org/docs/current/sql-notify.html
- tokio broadcast / watch: https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html , https://docs.rs/tokio/latest/tokio/sync/watch/index.html
- object_store Signer / SignedUrlOptions: https://docs.rs/object_store/0.14.2/object_store/signer/trait.Signer.html , https://docs.rs/object_store/0.14.2/object_store/signer/struct.SignedUrlOptions.html
- apalis README: https://raw.githubusercontent.com/apalis-dev/apalis/main/apalis/README.md ; apalis-postgres README/LICENSE: https://github.com/apalis-dev/apalis-postgres
- underway README: https://github.com/maxcountryman/underway
- tower-sessions-stores (README + sqlx-store Cargo.toml on main): https://github.com/maxcountryman/tower-sessions-stores
- openidconnect: https://docs.rs/openidconnect/4.1.0/openidconnect/ ; oauth2 5.0 Cargo features: https://docs.rs/crate/oauth2/5.0.0/features
- jsonwebtoken 11 README (crypto backend selection): https://github.com/Keats/jsonwebtoken
- GitHub Actions OIDC reference: https://docs.github.com/en/actions/reference/security/oidc
- Oso legacy library deprecation: https://github.com/osohq/oso , https://docs.rs/crate/oso-derive/latest/source/README.md
- utoipa CHANGELOG (6.0.0): https://raw.githubusercontent.com/juhaku/utoipa/master/utoipa/CHANGELOG.md
- memory-serve README: https://github.com/tweedegolf/memory-serve
- MDN CSP sandbox: https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/sandbox
- Google security blog, "Securely hosting user data in modern web applications": https://security.googleblog.com/2023/04/securely-hosting-user-data-in-modern.html
- GitLab HTML artifacts / separate domain discussions: https://gitlab.com/gitlab-org/gitlab-ee/issues/3864 , https://gitlab.com/gitlab-org/gitlab-foss/-/work_items/34102 , https://gitlab.com/gitlab-org/gitlab-ce/issues/3085
- Timescale/Tiger Data Postgres batch ingest benchmarks (UNNEST vs VALUES vs COPY): https://www.tigerdata.com/blog/benchmarking-postgresql-batch-ingest , https://www.timescale.com/blog/boosting-postgres-insert-performance
- Fork PR OIDC limitation (secondary sources, unverified against GitHub docs): https://github.com/github/docs/issues/33483 , https://redirect.github.com/codecov/codecov-action/pull/1054
- cargo-chef: https://github.com/LukeMathWalker/cargo-chef ; distroless: https://github.com/GoogleContainerTools/distroless

PoC (throwaway, not in repo): `/tmp/claude-0/-home-user/35b53bb7-a211-5ee3-8f5b-0d3a8129de43/scratchpad/server-poc` (`src/{main,live,storage,auth,store_session,jobs,api}.rs`). `cargo test` gives 5 passed, 1 ignored (needs DB).
