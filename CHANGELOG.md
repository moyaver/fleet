# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Relicensed from PolyForm Internal Use 1.0.0 to the GNU AGPL v3 (`AGPL-3.0-only`); a commercial license is available on request (ADR-0012).
- Renamed the product to Fleet; the agent binary is `fleet-agent` (ADR-0011).

### Added

- M3: the live hub and `/api/v1/live` (snapshot, then deltas in version order per attempt, `resync` for slow subscribers, `Origin` check), REST snapshots with a consistent `version` (`/api/v1/projects`, `…/attempts`, `…/attempts/{id}`), `/api/v1/openapi.json`, liveness (`online`/`stale`/`disconnected`/`completed`) and leader-only sweeps that detach silent runs and abandon expired ones (open flows become `unknown` from `server_inferred`, jobs `interrupted`), and Fleet plans (`POST /api/v1/plans`, immutable, planned totals before any job starts; #21). Reads need a viewer token until browser SSO (M4). The plan manifest type moved to `fleet-protocol::plan`.
- M3: the server ingests agents: `/api/v1/agent/ws` (`fleet.v1+json`: welcome with server-authoritative resume, ack after commit, `event_rejected`, `seq_gap`, fencing, pings, idle timeout, `goaway` + close 1012 on shutdown), the HTTPS batch fallback `POST /api/v1/agent/batch`, telemetry checkpoints with `telemetry_ack`, API-key auth, `/livez` and `/readyz`, and the `serve`, `migrate`, `check-config`, `healthcheck` and `api-key create` subcommands with figment configuration (TOML, `FLEET_*`, `*_FILE`).
- M3: `fleet-store` with migrations 0001–0005 (Postgres 18) and the ingest transaction (epoch fencing, duplicate skip, ownership-checked upserts, claim-lattice outcomes, recorded gaps); `fleet-core::outcome` (suite and job outcomes); `cargo xtask sqlx`; CI runs the database tests against a Postgres 18 service.
- Protocol: `flow.ended` may name its flow (`flow`), so a claim about a flow that never started (a skipped `executionOrder` flow) creates it.
- M2: the `fleet-agent` CLI (`run`, `inspect`, `flush`, `check`, `version`): supervisor, live observer, spool, WebSocket transport, metrics, adb tracking, planner shards; `fleet-loadgen mock-server`; cargo-dist release pipeline.
- M1: `cargo xtask fixtures capture | import`, 24 Maestro 2.12.1 fixtures, Phase 0 flows, research 08; ADR-0003 accepted.
- M0 repository bootstrap: Cargo workspace skeleton, `cargo xtask` (setup, ci, lint, web), CI and CLA workflows,
  licensing and governance files, mdBook skeleton, and a Leptos hello page served by `fleet serve`.
