# ADR-0005: Wire protocol

**Status:** Accepted (shipped in M2 and M3, 2026-11-06) · **Date:** 2026-11-03 · **Research:** [05 §3–8](../../research/05-protocol-and-data-model.md)

## Decision
- **Identities:** job (CI natural key, `check_run_id` on GitHub) ⊃ agent run (UUIDv7 made by the agent, owns `seq`) ⊃ connection (server-fenced `epoch`) ⊃ event `(agent_run_id, seq)`. A job may have several agent runs.
- **Encoding:** JSON text frames, subprotocol `fleet.v1+json`, `Authorization: Bearer` on the upgrade request, JSON Schema generated from Rust types. Envelope versions and per-event-type versions are kept separate.
- **Two streams:**
  - *Journal:* gap-free `seq`, spooled to disk, cumulative ack after Postgres commit, `journal.gap` marker on overflow.
  - *Telemetry:* own `tseq`, may be coalesced or dropped, acked at a 10–30 s checkpoint.
- **Resume:** the server's `welcome.committed_through` is authoritative, not the agent's last-seen ack.
- **Outcomes are claims** with provenance (`junit_final > artifact_file > live_console > process_exit > server_inferred`). The merge is order-independent; inferred sources can never claim `passed` (enforced by a DB constraint).
- **Keepalive:** ping every 15–25 s (proxy idle timeouts are 60–100 s). Heartbeat expiry > 3× the interval. HTTPS batch-POST fallback. Shipped: the agent pings every 10 s (`PING_INTERVAL` in `crates/fleet-agent/src/transport.rs`), and the server pings every `ingest.ping_interval_ms` (20 s by default).
- Artifacts go over HTTPS uploads, never WebSocket frames.

## Consequences
Exactly-once *effect* in the database; duplicates and reordering are harmless. An "agent session" no longer needs to equal a job.

## Amendment (2026-11-04)
`seq` is gap-free except for holes covered by a `journal.gap` marker. When the agent cannot store events (spool or send window exhausted, I10) it keeps their `seq` numbers, leaves them out, and sequences one `journal.gap{from_seq, to_seq}` at `seq == to_seq`, the last dropped slot. `from_seq..=to_seq` covers the dropped events and the marker's own slot. The server accepts such a marker when `from_seq == committed_seq + 1` and `to_seq == seq`, and advances `committed_seq` to `to_seq`; any other event that skips ahead is still a `seq_gap`. There is one `seq` space everywhere (session, spool, wire, server), so `ingest_gaps` and the stored events use the same numbers.
