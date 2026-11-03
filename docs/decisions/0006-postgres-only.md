# ADR-0006: PostgreSQL 18 only; native partitioning

**Status:** Proposed · **Date:** 2026-11-03 · **Research:** [05 §9–11](../../research/05-protocol-and-data-model.md), [03 §4–6](../../research/03-rust-server.md)

## Decision
- **PostgreSQL 18** (18.6 at time of writing; PG 19 GA is planned for 2026-11-19). Use native `uuidv7()`.
- **Partitioning:** the application manages native partitions for `agent_events`, `step_executions`, `host_samples` and `device_samples`. No TimescaleDB, no pg_partman.
- **Rollups:** 1-minute and 1-hour, built by an idempotent background job.
- **Retention:** 7 d raw 5 s samples, 90 d 1-min, 1 y aggregates. Retention works by dropping partitions.
- **Background work:** tokio intervals on an advisory-lock leader, plus a `SKIP LOCKED` jobs table.
- **Multi-instance fan-out** later: LISTEN/NOTIFY behind an `EventBus` trait. No Redis or Kafka.
- **Live-state versions** are persisted in Postgres, inside the ingestion transaction.

## Consequences
Worst case (50 workers × 4 devices): ~0.76 GB/day raw telemetry; ~8.7 GB for 90 d of 1-min rollups. The Postgres commit rate is the main bottleneck, so batch per socket and use UNNEST/COPY for telemetry.
