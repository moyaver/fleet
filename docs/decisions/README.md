# Architecture decision records

Short records of significant decisions. Status is one of **Proposed**, **Accepted**, **Superseded**.
Most entries are *Proposed*: they follow from the research in [`../../research/`](../../research/00-synthesis.md)
and should be confirmed by Phase 0 results or by the team.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-rust-for-agent-server-protocol.md) | Rust for agent, server and shared protocol | Accepted |
| [0002](0002-agent-supervisor-observer.md) | Agent: supervisor/observer process split, fail-open | Proposed |
| [0003](0003-live-signal-strategy.md) | Live progress from `maestro.log` + bundle folders + stdout | Proposed (Phase 0 gate) |
| [0004](0004-frontend.md) | Dashboard: Leptos CSR, all Rust, own component kit | Accepted |
| [0005](0005-wire-protocol.md) | Wire protocol: JSON journal + telemetry streams, server-authoritative resume | Proposed |
| [0006](0006-postgres-only.md) | PostgreSQL 18 only, native partitioning; no TimescaleDB, Redis or Kafka | Proposed |
| [0007](0007-object-storage.md) | Object storage: `object_store` crate; Garage default; filesystem backend | Proposed |
| [0008](0008-ci-publisher-auth.md) | CI publisher auth via OIDC on immutable claims; fork PRs offline | Proposed |
| [0009](0009-scope-focused-build.md) | Scope: focused build; thin history and analytics | Proposed |
| [0010](0010-licensing.md) | Source-available licensing (PolyForm Internal Use), CLA, donations | Accepted |
| [0011](0011-name-and-runner-scope.md) | Name "Fleet"; generic core, Maestro first | Accepted |
