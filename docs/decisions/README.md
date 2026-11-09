# Architecture decision records

Short records of significant decisions. Status is one of **Proposed**, **Accepted**, **Superseded**.
They follow from the research in [`../../research/`](../../research/00-synthesis.md). An ADR becomes **Accepted** when the owner merges the change that records it ([CONTRIBUTING §5](../../CONTRIBUTING.md#5-architecture-decisions)).

| ADR | Title | Status |
|---|---|---|
| [0001](0001-rust-for-agent-server-protocol.md) | Rust for agent, server and shared protocol | Accepted |
| [0002](0002-agent-supervisor-observer.md) | Agent: supervisor/observer process split, fail-open | Accepted (shipped in M2) |
| [0003](0003-live-signal-strategy.md) | Live progress from `maestro.log` + bundle folders + stdout | Accepted (Phase 0 passed) |
| [0004](0004-frontend.md) | Dashboard: Leptos CSR, all Rust, own component kit | Accepted |
| [0005](0005-wire-protocol.md) | Wire protocol: JSON journal + telemetry streams, server-authoritative resume | Accepted (shipped in M2 and M3) |
| [0006](0006-postgres-only.md) | PostgreSQL 18 only, native partitioning; no TimescaleDB, Redis or Kafka | Accepted (shipped in M3) |
| [0007](0007-object-storage.md) | Object storage: `object_store` crate; Garage default; filesystem backend | Accepted (shipped in M3 and M5) |
| [0008](0008-ci-publisher-auth.md) | CI publisher auth via OIDC on immutable claims; fork PRs offline | Accepted (shipped in M3) |
| [0009](0009-scope-focused-build.md) | Scope: focused build; thin history and analytics | Accepted (shipped in M5) |
| [0010](0010-licensing.md) | Source-available licensing (PolyForm Internal Use), CLA, donations | Superseded by 0012 |
| [0011](0011-name-and-runner-scope.md) | Name "Fleet"; generic core, Maestro first | Accepted |
| [0012](0012-agpl-license.md) | Open source under AGPL-3.0-only; CLA kept for a commercial license | Accepted |
| [0013](0013-fleet-planner.md) | Fleet Planner: static, duration-aware CI sharding | Accepted |
| [0014](0014-runner-neutral-names.md) | Runner-neutral names on the wire and in the schema | Accepted |
| [0015](0015-coordinate-mode.md) | Coordinate mode is not built; observe mode and the static planner stay | Accepted (rejected under the 20% rule) |
