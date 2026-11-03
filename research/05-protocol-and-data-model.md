# 05 — Agent↔Server Protocol, State Model and Database Schema

**Research date:** 2026-11-03
**Scope:** wire protocol between `fleet-agent` and `fleet-server` (WebSocket), event model, server-side state machines, JUnit reconciliation, PostgreSQL schema (operational, telemetry, analytics, artifacts, tenancy).
**Out of scope:** web framework, WebSocket library, ORM/driver and other crate choices (other researchers cover these). Crates are named here only where they bear on protocol semantics.

Every claim below carries one of these labels:

| Label | Meaning |
|---|---|
| **[V]** | Checked during this research against a primary source (official docs, RFC, upstream source code). Links are in §16. |
| **[K]** | Prior knowledge that is well established but was not re-checked in this session. Confirm before relying on it. |
| **[P]** | A design proposal from this report. It is not a fact about any external system. |

---

## 0. Executive summary

1. **Identity layers [P].** Keep these four apart: the **job** (a CI natural key that stays the same across agent restarts), the **agent run** (one `maestro-fleet run` process, identified by an agent-generated UUIDv7; this is the spec's "agent session"), the **connection** (one WebSocket, which the server fences with an `epoch` counter), and the **event** (`(agent_run_id, seq)`). The spec's single "agent session" covers all four, and that is the root of most of the gaps listed in §14.
2. **Two streams on one socket [P].** A *journal* stream carries lifecycle events and evidence. It is gap-free, sequenced, durably acked and replayed after reconnect. A *telemetry* stream carries metric samples. It has its own sequence and may be coalesced or dropped under pressure. The two need separate streams because the spec allows metric samples to be discarded, and dropping samples from a shared sequence would leave gaps in it.
3. **The server is authoritative for the resume point [P].** On reconnect the server's `welcome` returns `committed_through`. An agent's own "last acked seq" can be stale, because a commit can succeed and its ack still be lost (MQTT and Kafka handle this the same way).
4. **Acks are cumulative and sent after commit; dedup happens in the same transaction [P].** Each batch is applied in one Postgres transaction that also advances `agent_runs.committed_seq`. That gives an exactly-once *effect* in the database. Agent-generated entity IDs plus `ON CONFLICT` give a second, independent line of idempotency.
5. **Encoding is JSON over WebSocket text frames [P].** Types are defined in a shared `fleet-protocol` crate with serde, and JSON Schema 2020-12 is generated with schemars 1.x. Traffic is small, at most a few hundred events per second fleet-wide (§10.3). Binary encodings save nothing that matters and cost debuggability. bincode is now unmaintained (RUSTSEC-2025-0141) **[V]**.
6. **Evidence and outcome are stored separately [P].** Every flow outcome is *derived* from a set of **claims**. Each claim has a provenance (`junit_final`, `artifact_file`, `live_console`, `process_exit`, `agent_observed`, `server_inferred`, `declared`). The merge is an order-independent join (a semilattice), so duplicates and reordering cannot corrupt the result. Only sources that directly observe a flow may claim `passed`. Inferred sources can claim only `interrupted` or `unknown`.
7. **JUnit reconciliation follows Maestro's actual report shape [V].** In Maestro 2.12.x the report is written **once**, after all shards finish. It holds one `<testsuite>` per shard, in shard order, with test cases in execution order. The `status` attribute is non-standard (`SUCCESS`/`ERROR`/`WARNING`/`CANCELED`/`STOPPED`/…). Timestamps are local time with no UTC offset, truncated to whole seconds. So matching should be positional per shard first and fall back to name or file only when needed. That approach handles duplicate flow names and `--shard-all` (§8).
8. **PostgreSQL [V]: target PG 18 (current minor 18.6).** It provides native `uuidv7()`, OLD/NEW in `RETURNING` and skip scan. PG 19 is at RC1, with GA planned for 2026-11-19. Its `INSERT … ON CONFLICT DO SELECT` is useful, but v1 should not depend on it.
9. **High-volume tables are partitioned natively, without TimescaleDB or pg_partman [P].** These are `agent_events`, `step_executions`, `host_samples` and `device_samples`. The application creates and drops partitions. Journal tables are partitioned by `run_day`, a column fixed per agent run, so their primary key stays globally unique. Telemetry is partitioned by sample time. 1-minute and 1-hour rollups are built by an idempotent background job that tracks late data. Upper-bound volume is about 0.76 GB/day of raw 5-second telemetry and about 8.7 GB for 90 days of 1-minute rollups (§10).
10. **Artifacts use content-addressed blobs scoped per project [P].** Each object lives at `sha256` within its project. A logical `artifacts` table references the blobs. A *retention class is decided after the flow outcome is known*: a screenshot is promoted from `success_evidence` to `failure_evidence` when the flow fails. GC is reference-based with a grace period. Dedup is scoped per project so that cross-tenant probing for existing hashes is not possible.

---

## 1. Verified baseline facts used by this design

| Topic | Fact | Label |
|---|---|---|
| PostgreSQL versions | Supported: 18 (18.6), 17 (17.11), 16 (16.15), 15 (15.19), 14 (14.24, EOL **2026-11-12**). PG 18 was first released 2025-09-25 and reaches EOL 2030-11-14. | [V] |
| PostgreSQL 19 | RC1 posted 2026-10-15. RC2 is planned for 2026-11-12 and GA for **2026-11-19**. Relevant additions: `INSERT … ON CONFLICT DO SELECT … RETURNING`, `COPY TO` for partitioned tables, `COPY … FORMAT json`, default TOAST compression changed to lz4, `REPACK [CONCURRENTLY]`, parallel autovacuum, casts between bytea and uuid. | [V] |
| `uuidv7()` | PG 18 provides `uuidv7([shift interval])`, `uuidv4()` and `uuid_extract_timestamp()`. The commit message says values are monotonic **within one backend**, using 12 bits of sub-millisecond precision (RFC 9562 method 3). Values from different connections are not mutually ordered. | [V] |
| PG 18 other | Async I/O (`io_method`), B-tree skip scan, virtual generated columns as the default, OLD/NEW in `RETURNING`, `WITHOUT OVERLAPS` / `PERIOD` temporal constraints, `NOT VALID` FKs on partitioned tables, planner statistics kept across `pg_upgrade`. | [V] |
| Partitioning limitation | A unique or primary key on a partitioned table must include all partition-key columns, with no expressions. The planner copes with "up to a few thousand partitions" as long as queries can prune. `DETACH PARTITION … CONCURRENTLY` exists. | [V] |
| pg_partman | Latest release is 5.5.0 (22 Jul). 5.4.0 added `create_partition()` to replace `create_parent()`. It is an extension and needs a custom image or a managed service that ships it. | [V] (the year was not shown on the releases page) |
| Rust `uuid` | `Uuid::now_v7()`: "All UUIDs generated through this method by the same process are guaranteed to be ordered by their creation." Current crate version is 1.29.0. | [V] |
| schemars | Version 1.2.3. Emits JSON Schema draft 2020-12 and honours `#[serde(...)]` attributes. | [V] |
| bincode | 3.0.0 (2025-12-16) is a tombstone release: development has ceased and RUSTSEC-2025-0141 marks it unmaintained. | [V] |
| Maestro | Latest CLI is 2.12.1 (02 Nov). JUnit reporter source was read at `main` (§8.1). | [V] |
| GitHub Actions IDs | `GITHUB_RUN_ID` does not change on re-run. `GITHUB_RUN_ATTEMPT` identifies the attempt. `GITHUB_JOB` is the job's YAML key, which is **not unique across matrix legs**. No default env var exposes a numeric job ID, but the `job.check_run_id` expression context and the OIDC claim `check_run_id` ("the check run ID of the current job") do. The OIDC token also carries `repository`, `repository_id`, `run_id`, `run_attempt`, `ref`, `event_name` and `runner_environment`. | [V] |
| nginx | By default nginx closes a proxied WebSocket after 60 s with no data from upstream. Server-side ping frames prevent this. | [V] |
| S3 | `If-None-Match: *` conditional PUT returns 412 if the key already exists. SHA-256 checksums are supported and validated on upload. Support across S3-*compatible* stores varies, so do not depend on it. | [V] for AWS, [K] for other stores |

---

## 2. Survey: proven reliable-delivery designs and what to take from each

| System | Mechanism | What applies to Fleet | What does not |
|---|---|---|---|
| **MQTT 5 QoS 1** [V] | Each in-flight PUBLISH has a non-zero Packet Identifier that may be reused after PUBACK. Session state survives disconnects for the *Session Expiry Interval*. CONNACK carries *Session Present*. A client that has state but receives Session Present = 0 must discard it. *Receive Maximum* caps in-flight QoS 1/2 messages (default 65,535). The server closes the connection after 1.5× Keep Alive with no traffic. Re-delivered PUBLISHes set DUP = 1. A server can redirect with reason 0x9C/0x9D plus Server Reference. | Session resume with an explicit server answer ("I have your session / I do not"). An in-flight window for flow control. The 1.5× keepalive rule. A redirect/"go away" message. Session takeover: a newer connection for the same identity displaces the old one. | Per-message packet IDs that get reused. Fleet needs a durable total order for audit and replay, so it uses a monotonic `seq` instead. |
| **Kafka idempotent producer (KIP-98)** [V] | Each producer gets a PID and an epoch. Sequence numbers are per partition. The broker accepts only `last+1`. A lower sequence is a duplicate and is ignored. A higher sequence is an out-of-order error and is fatal. A bumped epoch fences zombie producers. Idempotence holds *only within one producer session*. | `seq` must be exactly `committed+1`. A lower `seq` is acked and discarded. A higher `seq` is a protocol error and triggers a resend. **Epoch fencing** of stale connections (§3.7). The scope limit is the important lesson: a restarted agent is a new producer, so cross-process continuity needs an explicit link (`previous_agent_run_id`) or a persisted spool. | Transactions across partitions. |
| **NATS JetStream** [V]/[K] | Deduplication by a `Nats-Msg-Id` header within a *duplicate window*. Secondary sources give the default as 2 minutes; the official default was not confirmed. A PubAck returns the stream seq and a `duplicate` flag. Expected-last-sequence headers act as optimistic concurrency. | Ack frames can say "duplicate" so agent metrics can count replays. Time-windowed dedup is the cautionary tale: it is **not enough** for Fleet, where a replay can arrive hours later from a spool upload. Fleet's dedup is therefore keyed by `seq` and kept indefinitely through the cursor and unique keys. | A time-bounded dedup window. |
| **Socket.IO connection-state recovery** [V] | The server issues a private session ID plus a per-packet offset. The client returns both on reconnect, and the server replays missed packets for up to `maxDisconnectionDuration`. `socket.recovered` reports success. The docs say recovery "will not always be successful" and that applications must still resync. | A bounded *resume grace* for the **live state** (worker shows "disconnected", not "lost"). The application must always cope with a failed recovery, which is the spec's "recover with a fresh snapshot" rule for browsers. | Socket.IO replays server→client traffic. Fleet's reliable direction is agent→server. |
| **Phoenix Channels** [V] | Messages are `[join_ref, ref, topic, event, payload]`. A heartbeat on topic `phoenix`. Delivery to clients is **at-most-once**. Clients rejoin topics after reconnect with exponential backoff. | A `ref` for correlating requests and replies (used here for `upload_request` → `upload_grant`). An app-level heartbeat. At-most-once is acceptable for the **browser fan-out**, which resyncs from snapshots, and **unacceptable for agent ingestion**. | — |
| **OTLP (OpenTelemetry)** [V] | Retryable: gRPC UNAVAILABLE, DEADLINE_EXCEEDED and others; HTTP 429/502/503/504. Not retryable: HTTP 400 and other 4xx. Throttling is signalled with `RetryInfo` or `Retry-After`. **`partial_success`**: the server reports a rejected count, and the client must not retry. Exponential backoff with jitter. Duplicates after an uncertain delivery are an accepted trade-off. | (a) **Poison events must not wedge the stream.** The server records the rejection, advances the cursor and tells the agent *permanent, do not retry* (`event_rejected{permanent:true}`). (b) Explicit throttle with a retry-after value. (c) Backoff with jitter. (d) Telemetry may tolerate duplicates. Fleet makes telemetry idempotent anyway with `PK(agent_run_id, ts)`. | — |
| **Graphite plaintext** [K] | Fire-and-forget lines over TCP, with no acks. | Confirms that metric samples can be lossy as long as gaps are *visible* (see `coalesced_n` and the telemetry gap markers). | Everything else. |

**What the design takes from all of this [P]:**

* A total order per producer, with the server accepting exactly `next` (Kafka).
* An explicit resume answer from the server (MQTT Session Present). The resume point comes from the server, not the client.
* Fencing of zombie connections with an epoch (Kafka, MQTT session takeover).
* An in-flight window and an explicit throttle (MQTT Receive Maximum, OTLP Retry-After).
* Poison-pill rejection that advances the cursor (OTLP partial success).
* Lossy-but-labelled telemetry (Graphite).
* Snapshot-based resync for browsers (Socket.IO, Phoenix).

---

## 3. Agent↔server protocol (proposal)

### 3.1 Identity layers [P]

| Layer | Identifier | Who generates it | Lifetime | Why it is needed |
|---|---|---|---|---|
| Project | `project_id` | Server | Permanent | Tenancy |
| Pipeline run | `(provider, repo_id, run_id)` → `pipeline_run_id` | Natural key from CI; surrogate from server | Permanent | Groups re-runs |
| Pipeline attempt | `(pipeline_run_id, run_attempt)` | CI and server | Permanent | "Pipeline ID must distinguish attempts" (§7 of the spec) |
| Job | `(pipeline_attempt_id, job_key)` → `job_id` | CI and server | Permanent | Stays the same across reconnects and **across agent restarts within one attempt** |
| Agent run ("session") | `agent_run_id` (UUIDv7) | **Agent**, once per `maestro-fleet run` process | One process | Owns the `seq` space. A job can have more than one. |
| Connection | `connection_id` (server UUIDv7) + `epoch` (int) | Server | One socket | Logs, fencing |
| Journal event | `(agent_run_id, seq)` | Agent | Permanent | Dedup and order. **No separate `event_id` is needed.** |
| Execution entities | `suite_execution_id`, `shard_id`, `device_session_id`, `flow_execution_id`, `artifact_id` (UUIDv7) | **Agent** | Permanent | Idempotent upserts. The server's dedup state is not required for correctness. |
| Report-derived entities | Deterministic UUIDv8 = `H(suite_execution_id ‖ shard_index ‖ testcase_position)` | Agent or server reconciler | Permanent | Re-running reconciliation yields the same IDs |

**`job_key` derivation [P]:**

1. If set, use `FLEET_JOB_KEY`. The docs should recommend `${{ job.check_run_id }}` on GitHub.
2. Otherwise use the OIDC `check_run_id` claim when OIDC is in use. This is authenticated, so it is preferred.
3. Otherwise fall back to `GITHUB_JOB` plus `sha256(matrix JSON)`, where the matrix JSON must be passed as `FLEET_MATRIX: ${{ toJSON(matrix) }}`.
4. If none of these is available, use the job key plus a warning that matrix legs may collide.

**[V] basis:** `GITHUB_JOB` is not unique across matrix legs, and `check_run_id` exists both as a context and as an OIDC claim.

### 3.2 Two logical streams over one socket [P]

| | Journal stream | Telemetry stream |
|---|---|---|
| Content | Lifecycle events, observations/claims, artifact registration, warnings | Host, process and device metric samples |
| Sequence | `seq` u64. Starts at 1, gap-free, assigned when the event is enqueued into the outbox. | `tseq` u64, independent. Gaps are allowed and reported. |
| Agent buffer | File-backed spool (bounded, default 64 MiB) with an in-memory tail | In-memory ring (default 2 MiB). Coalesces 5-second samples into 1-minute aggregates, then drops the oldest. |
| Ack | `ack{through}` after the Postgres commit | `telemetry_ack{through}` after the batch flush, which the server runs every 10–30 s. This is the spec's "durable metrics checkpoint". |
| Overflow policy | Coalesce step events first (drop `step.started` when the matching `step.ended` is queued). Then emit `journal.gap{from,to,reason}` as a *sequenced* marker so the loss is explicit. **Never** drop silently. | Coalesce, then drop. Each batch carries `dropped_since_last` so the server records telemetry gaps. |

### 3.3 Frames

Each WebSocket text frame carries one JSON object with an `op` discriminator. Subprotocol: `Sec-WebSocket-Protocol: fleet.v1+json`. Authentication uses `Authorization: Bearer …` on the upgrade request. The agent is not a browser, so it can set headers.

**Client → server**

| op | Purpose |
|---|---|
| `hello` | Protocol range, agent build, `agent_run_id`, optional `previous_agent_run_id`, CI context, resume state (`next_seq`, `oldest_unacked_seq`, `telemetry_next`), clock sample, capabilities |
| `events` | Batch of journal envelopes (1..N), strictly contiguous in seq |
| `telemetry` | Batch of samples with `tseq_from..tseq_to` and `dropped_since_last` |
| `heartbeat` | Clock sample, queue depth, spool bytes, Maestro process alive flag |
| `upload_request` | `ref`, artifact metadata, sha256, size → server replies `upload_grant` |
| `upload_complete` | `ref`, `artifact_id` → server verifies the object |
| `bye` | Graceful end after `run.ended` has been acked. The agent then closes with 1000. |

**Server → client**

| op | Purpose |
|---|---|
| `welcome` | Chosen protocol version, `connection_id`, `epoch`, **`committed_through`**, `telemetry_committed_through`, `resume: new\|resumed\|reopened`, limits (`max_inflight_events`, `max_frame_bytes`, `heartbeat_interval_ms`, `idle_timeout_ms`), effective policy (metric interval, artifact allowlist and size caps), server time |
| `reject` | Handshake refused: `code`, `retry_after_ms`, `fatal` |
| `ack` | `{through, epoch, duplicates}`. Cumulative and sent after commit. |
| `telemetry_ack` | `{through}` |
| `event_rejected` | `{seq, code, permanent}`. The event is recorded in `ingest_rejections` and the cursor advances. The agent must not resend it. |
| `throttle` | `{max_inflight_events?, telemetry_interval_ms?, pause_telemetry_ms?, retry_after_ms?}` |
| `snapshot` | Informational view of the authoritative state (job lifecycle, agent-run state, server-inferred overrides on open flows, active policy). **Agent correctness never depends on it** (§3.6). |
| `upload_grant` | `{ref, artifact_id, disposition: upload\|exists\|rejected, url?, headers?, expires_at?}` |
| `goaway` | `{reason: shutdown\|rebalance\|credential_expiring\|policy, reconnect_after_ms, endpoint?}`. Equivalent to MQTT 0x9C/0x9D or HTTP/2 GOAWAY. |
| `error` | `{code, fatal, message}`. Fatal errors are followed by a close. |

The keepalive also uses **WebSocket Ping** control frames from the server every 20 s, which keeps the connection under the 60 s nginx default.

### 3.4 Handshake and resume sequence

```text
Agent                                             Server
  | WSS upgrade (Bearer token, subprotocol fleet.v1+json)
  |------------------------------------------------->| authn: credential → project; OIDC claims → CI natural keys
  | hello{agent_run_id=R, proto{min:1,max:1},
  |       resume{next_seq:212, oldest_unacked:180},
  |       ci{...}, clock{wall,mono_ms}}              |
  |------------------------------------------------->| tx: upsert pipeline_run/attempt/job by natural key;
  |                                                  |     INSERT agent_runs(R) ON CONFLICT DO NOTHING;
  |                                                  |     verify R.project = credential.project and R.job = job;
  |                                                  |     UPDATE agent_runs SET epoch=epoch+1 RETURNING epoch, committed_seq
  |<-------------------------------------------------| welcome{epoch:7, committed_through:195, resume:"resumed", limits, policy, server_time}
  | drop outbox ≤195; resend 196..211 in order       |
  |------------------------------------------------->| events{196..211} → apply tx → ack{through:211, epoch:7}
  |<-------------------------------------------------|
  |  ...steady state: events, telemetry, heartbeats   |
```

**Resume rules [P]:**

* **`committed_through ≥ oldest_unacked − 1`** is the normal case. The agent discards everything up to `committed_through` and resends the rest.
* **`committed_through < oldest_unacked − 1`.** The agent no longer holds events the server never committed, for example because the spool was lost or truncated. The server records an `ingest_gap(R, committed+1 .. oldest_unacked−1, reason:"agent_discarded")` and fast-forwards the cursor. The gap is shown as missing evidence. *Rationale:* a gap must never wedge ingestion.
* **`committed_through ≥ next_seq`.** This is impossible unless the agent reused an `agent_run_id`. The server answers `reject{code:"seq_regression", fatal:true}`. The agent then starts a new run with `previous_agent_run_id = R`.
* **Unknown `R` with `next_seq > 1`.** For example, the server's database was restored from a backup. The server answers `resume:"new"` with `committed_through: 0`. The agent replays what it still has, and any missing prefix becomes a recorded gap. Idempotent entity upserts keep this safe.
* **`R` is in state `abandoned`** because its grace expired. The server accepts the connection (`resume:"reopened"`) and flags `late_resume`. Late evidence is still evidence (§6).
* **`R` is in state `ended`.** The server accepts only to re-ack. Events with `seq > end_seq` are rejected as `after_end`.

### 3.5 Ack strategy

| Option | Pros | Cons | Decision |
|---|---|---|---|
| Per-event ack before commit | Low latency | Not durable. Violates "the server acknowledges durable ingestion". | ✗ |
| Per-event ack after commit | Precise | One commit per event (fine at ~4 ev/s per agent) and chatty | Acceptable but unnecessary |
| **Cumulative `ack{through}` after the commit of each batch** | Simple, TCP-like, and a lost ack is repaired by the next one | One rejected event needs a side channel (`event_rejected`) | **✓ [P]** |

* **Agent batching:** flush when 64 events are queued, when 20 ms of linger has passed, or immediately for latency-critical events (`flow.started`, `flow.ended`, `run.*`). The in-flight window is `max_inflight_events` (default 512) and the frame cap is `max_frame_bytes` (default 1 MiB).
* **Server:** a single writer per agent run, namely that run's connection task. Each frame is applied in one transaction (§9.4) and then acked. Group commit across connections is unnecessary at this scale (§10.3).
* **Duplicates** (`seq ≤ committed`) are acked again without being applied, and counted in `ack.duplicates`.
* **Gaps** (`seq > committed+1`) produce `error{code:"seq_gap", expected}` without closing the connection. The agent restarts its send cursor at `expected`. Over one TCP stream this should only happen because of an agent bug.

### 3.6 What "receive the current authoritative state" (spec §5 step 5) should mean [P]

The spec does not say what this state contains or what the agent does with it. The proposal:

* The `snapshot` frame is **advisory**. It tells the agent whether the server inferred something while the agent was away, for example `agent_run.state = abandoned` and flows set to `unknown(server_inferred)`. The agent prints this in the CI log. It need not send corrections, because its later events outrank inferred claims (§7).
* It also carries the *effective policy* (metric interval, artifact allowlist, size limits). The server can push a new `snapshot.policy` at any time to throttle or tighten policy.
* The agent must **never** treat the snapshot as a source of test truth.

### 3.7 Fencing zombie connections [P]

Two sockets can claim the same `agent_run_id`. This happens with a half-open TCP connection after a network flap, or when two server instances sit behind a load balancer.

1. `hello` runs `UPDATE agent_runs SET epoch = epoch + 1 … RETURNING epoch`. The connection keeps `my_epoch`.
2. Every apply transaction ends with:

   ```sql
   UPDATE agent_runs SET committed_seq = $new, last_frame_at = now()
   WHERE id = $run AND epoch = $my_epoch AND committed_seq = $prev;
   ```

   If no row is updated, the transaction rolls back, the server sends `error{code:"superseded", fatal:true}` and closes with 1008.
3. This makes Redis or sticky sessions unnecessary for correctness. Fan-out to browsers across instances is a separate concern; see spec §6, optional Redis.

This is optimistic concurrency, comparable to Kafka epoch fencing and MQTT session takeover. It requires no long-held advisory locks or a pinned Postgres connection per agent.

### 3.8 Keepalive, liveness and timeouts [P]

| Parameter | Default | Notes |
|---|---|---|
| Server WebSocket Ping | 20 s | Below nginx's 60 s idle cut [V] |
| Agent `heartbeat` frame | 10 s | Carries a clock sample and queue depth. Telemetry and event frames also count as liveness. |
| `stale` threshold (display) | 25 s since the last inbound frame | Worker shows "stale". Data-freshness badges turn amber. |
| Idle timeout (server closes the socket) | 45 s with no inbound frame | About 1.5× the heartbeat-plus-telemetry cadence, in the spirit of the MQTT 1.5× rule |
| Resume grace | 15 min (configurable per project) | `agent_run: detached → abandoned` when it expires |
| Late-data acceptance | Unlimited while the partitions exist | Spool uploads after the job finishes are allowed |
| Agent reconnect backoff | Exponential with full jitter, 250 ms base, 30 s cap. Honour `retry_after_ms` / `reconnect_after_ms`. | Never block Maestro output reading. Never fail the CI job. |

### 3.9 Graceful server shutdown and redeploy [P]

1. Stop accepting new upgrades (readiness turns false).
2. Send `goaway{reason:"shutdown", reconnect_after_ms: rand(0..2000)}` to every connection.
3. Stop reading new `events` frames. Finish in-flight transactions, send the final `ack` and flush telemetry plus `telemetry_ack`.
4. Close with 1012 (service restart) [K]. Agents reconnect, possibly to another instance, with jitter. Resume works because all state is in Postgres.
5. Live projections are rebuilt from Postgres on start. That covers spec §13, "backend restart".

### 3.10 WebSocket close codes [K]

1000 normal (after `bye`) · 1001 going away · 1008 policy violation (authentication, scope, superseded) · 1009 message too big · 1011 internal error · 1012 service restart · 1013 try again later (overload). Codes 1012 and 1013 come from the IANA registry, not RFC 6455 itself.

### 3.11 Is "monotonic sequence per agent session plus idempotent apply" sufficient?

**It is sufficient for:** network drops, lost acks (as long as the server is the authority for the resume point), duplicate deliveries, and server restarts mid-batch, because the cursor is committed in the same transaction as the apply.

**It is not sufficient for these cases, so the proposal adds a fix for each:**

| Case | Problem | Fix [P] |
|---|---|---|
| Agent process restarts within a job | New process, new seq space. The old seq space is orphaned. Same limit as Kafka's "within one producer session". | New `agent_run_id` with `previous_agent_run_id`. The new process may replay the old run's spool over HTTP (`POST /api/v1/agent/runs/{old}/spool`) with the same seq dedup. The old run moves to `ended(superseded)`. |
| Two agents in one job (sequential smoke then regression, or parallel with `&`) | The spec says "each job establishes one connection" | Allow N agent runs per job, each with its own connection and seq space. Job state is an **aggregate over runs** (§5.3), never a counter updated in place. |
| CI re-run (`run_attempt` 2) | A new attempt must not merge with attempt 1 | New `pipeline_attempt` and new `job` rows. A flow's "attempt number" is derived across attempts (§9.2, `flow_attempt_groups`). |
| Runner lost with unacked events | Data loss is unavoidable, as the spec acknowledges | Spool on disk under the workspace. Optional `if: always()` CI step `maestro-fleet inspect --upload-spool`. The server marks missing finalization (`abandoned` → flows `unknown`). |
| Agent buffer full of lifecycle events | The spec says "preserve whenever possible" but does not define the case when that is impossible | Sequenced `journal.gap` marker. Gaps are visible in the UI as "missing evidence". |
| Metrics sharing the seq space | Dropping samples would leave seq gaps | Separate telemetry stream (§3.2) |
| Malicious or buggy agent reusing another project's entity IDs | `ON CONFLICT DO NOTHING` would silently attach to foreign rows | Every upsert verifies `(project_id, agent_run_id)` ownership on conflict. A mismatch produces a permanent rejection plus an audit event (§9.4). |
| Clock skew | `occurred_at` alone is unreliable | Use a wall-clock plus monotonic pair, and estimate the offset per run (§4.6) |

---

## 4. Serialization and schema evolution

### 4.1 Traffic profile (from §10.3)

Fleet-wide upper bound at 50 workers × 4 devices: about 207 journal events/s plus 50 telemetry samples/s. That is roughly 4–5 events/s per agent, or 80–100 KB/s fleet-wide in JSON before compression. At this volume, CPU and bandwidth costs of serialization do not matter. Debuggability, schema evolution and frontend reuse do.

### 4.2 Comparison

| Format | Size vs JSON | Self-describing | Evolution | Debuggability | Non-Rust/browser consumers | Rust maturity |
|---|---|---|---|---|---|---|
| **JSON (serde_json 1.0.154)** | 1× (about 0.2–0.3× with permessage-deflate [K]) | Yes | Additive fields are easy. Unknown fields are ignored by default. | Excellent: `websocat`, `jq`, logs, Postgres `jsonb` | Native | [V] |
| MessagePack (rmp-serde 1.3.1) | About 0.6–0.8× [K] | Yes | Same as JSON through serde | Needs tooling | Libraries in every language | [V] version |
| CBOR (ciborium 0.2.2) | About 0.6–0.8× [K] | Yes, IETF standard (RFC 8949) | Same as JSON | Needs tooling | Good | The crate has had no release since 2024-01 [V] |
| Protobuf (prost 0.14.4) | About 0.3–0.5× [K] | No (needs `.proto`) | Excellent field-number discipline | Poor without schemas | Excellent codegen | Schema-first. Rust types are generated, which conflicts with "Rust types are the source of truth". |
| postcard 1.1.3 | Smallest | No | Append-only at best. No field skipping. | Poor | Rust-centric | Designed for embedded and storage, not cross-version wire protocols |
| bincode | Small | No | None | Poor | — | **Unmaintained (RUSTSEC-2025-0141)** [V] |

**Recommendation [P]:** use **JSON over text frames** as protocol v1. Reserve a negotiated subprotocol `fleet.v1+msgpack` for later. Because both go through serde, adding it would not change the schema. The constraint is to keep the types serde-format-agnostic: avoid `#[serde(flatten)]` combined with `deny_unknown_fields`, and avoid relying on `serde_json::Value` inside typed payloads except at the raw-data boundary. Make permessage-deflate optional, depending on library support; that is a crate-research item.

### 4.3 Evolution rules [P]

1. **`proto` (connection level, integer)** is negotiated in `hello`/`welcome`. A breaking change to the envelope or framing bumps it. The server supports `[current−1, current]`.
2. **Per-type payload version `v`** (defaults to 1) replaces the spec's global `schema_version`. A breaking change to *one* event type increments its `v`, and the server keeps decoders for every `v` it has ever accepted. A breaking change is rare; prefer a new event type.
3. **Within a `v`, changes are additive only.** New fields must be `Option<…>` or `#[serde(default)]`. Never repurpose a field. Never change a unit or type: `duration_ms` stays milliseconds forever.
4. **Unknown fields are ignored and kept** inbound. Do not use `deny_unknown_fields`. The raw `data` is stored in `agent_events.data`, so later server versions can re-derive state from fields they did not understand at ingest.
5. **Unknown event types** are stored in the journal with `apply_status = 'unsupported'` and acked normally, never rejected. A newer agent can therefore talk to an older server. Agent warnings in the admin "agent compatibility" screen surface these.
6. **Unknown enum values** deserialize to an `Unrecognized` catch-all through `#[serde(other)]` for unit-variant enums. They map to `unknown` in derived state, **never** to `passed`.
7. **IDs are canonical lowercase UUID strings. Times are RFC 3339 UTC with millisecond precision. Durations are integer milliseconds. Sizes are integer bytes.** `seq` stays below 2^53, so it is safe in JavaScript.
8. **Schema artefacts.** CI generates `fleet-protocol/schema/agent-protocol.v1.schema.json` with schemars (draft 2020-12, which is OpenAPI 3.1-compatible [K]). A test fails if the committed schema differs from the generated one. Golden-fixture tests replay recorded journals from every supported agent version against the current server.
9. **Compatibility window.** The server accepts agent minor versions N−2. `welcome` can carry `agent_update_recommended`.

### 4.4 Tolerant envelope decoding in Rust [P]

To tolerate unknown types, decode in two stages. Parse the envelope first, with `data` kept as raw JSON, then dispatch on `(type, v)`. An adjacently tagged enum alone would fail on an unknown tag.

---

## 5. Event model

### 5.1 Critique of the spec's event list and example

| # | Issue in spec §5 | Consequence | Proposal |
|---|---|---|---|
| 1 | `event_id: "evt-000128"` duplicates `sequence` | Two dedup keys that can disagree | Identity is `(agent_run_id, seq)`. Drop `event_id`. |
| 2 | No agent-run or session ID in the envelope | `sequence` is ambiguous across agent restarts | `agent_run_id` is bound to the connection in `hello`. The journal stores it per row. |
| 3 | `pipeline_id: "pipeline-142"`, `job_id: "android-regression-1"` are human strings repeated in every event | Not unique: run numbers repeat across attempts and matrix legs. Spoofable. Redundant. | Send CI natural keys **once** in `hello`/`run.started`. The server binds them, preferring OIDC claims. Events carry only execution-entity IDs. |
| 4 | `device_id: "emulator-5554"` | ADB serials are **reused** by every job on a host and by concurrent emulators on different hosts | `device_session_id` (agent UUIDv7) is the identity. The serial is an attribute. |
| 5 | `flow_id: "kyc/address-verification.yaml"` | Mixes up the *definition* (path) with the *execution*. Shard-all runs the same path on N devices. | `flow_execution_id` (UUID) plus `flow: {path, name}` attributes. The server maps the path to `flow_definition_id`. |
| 6 | `attempt: 1` with no definition | Maestro has no CLI-level flow retry, and the spec excludes automatic retries as a non-goal, so what increments it? | Remove from the envelope. Attempts are derived (§9.2). |
| 7 | `flow.completed` and `flow.failed` are separate types | An outcome encoded in a type name multiplies types (skipped? interrupted?) | One `flow.ended{outcome}` |
| 8 | `"evidence": "live"` sits inside the payload | Provenance is optional and buried | A first-class `src` in the envelope (§5.3) |
| 9 | Only wall-clock `occurred_at` | Skew, NTP steps and no reliable durations | `at: {wall, mono_ms}` (§4.6) |
| 10 | `step: "assertVisible"` | Cannot identify which of several assertVisible steps, or a nested `runFlow` | `step: {index, path}`, where `path` is the index path through nested commands |
| 11 | Missing events | Cannot represent plan, report, exit, signals, uploads, gaps or parser drift | See the taxonomy below |

### 5.2 Refined taxonomy [P]

Events fall into five classes. The class determines which tables an event touches.

| Class | Types | Typical `src` | Effect on derived state |
|---|---|---|---|
| **A. Context** (declares entities and attributes) | `run.started`, `worker.identified`, `device.discovered`, `device.updated`, `suite.started`, `suite.planned`, `shard.assigned`, `flow.started`, `step.started` | `agent_observed`, `declared`, `live_console` | Creates or upserts entities. Never sets a terminal outcome. |
| **B. Observations / claims** (evidence about outcomes) | `step.ended`, `flow.ended`, `flow.reported` (one JUnit testcase), `report.parsed` (summary), `process.exited`, `device.state_observed`, `run.signal` | All provenances | Inserts a **claim**. The outcome is re-derived (§7). |
| **C. Artifacts** | `artifact.registered`, `artifact.uploaded`, `artifact.rejected` | `agent_observed` | Artifact metadata. May trigger a retention-class decision. |
| **D. Run lifecycle** | `run.finalizing` (Maestro exited, now reconciling and uploading), `run.ended{exit_code_returned, final_for_job?}` | `agent_observed` | Agent-run state machine (§6.2) |
| **E. Diagnostics** | `agent.warning`, `journal.gap`, `parser.unrecognized` (hash and count of unparsed console lines per parser version), `parser.disagreement` | `agent_observed` | Ingestion health. Maestro-compatibility alarms (spec §15). |

The spec's event names map as follows:

* `session.started` → `run.started`
* `worker.registered` → `worker.identified`
* `worker.metrics` → telemetry stream
* `device.status_changed` → `device.state_observed`
* `flow.completed`/`flow.failed` → `flow.ended`
* `suite.completed` → `process.exited` + `report.parsed` + `suite.ended`
* `session.completed` → `run.ended`
* `session.interrupted` → `run.signal` + `run.ended{interrupted:true}`, or the server-side `abandoned`

### 5.3 Provenance as a first-class field (spec §18) [P]

The spec's six categories map onto **two orthogonal fields**:

* `src.kind` (where the evidence came from)
* the derived `finality` (how settled the derived value is)

| Spec §18 category | Representation |
|---|---|
| Directly observed information | `src.kind ∈ {agent_observed, process_exit}` (ADB state, exit code, signal, file existence) |
| Provisional live information | `src.kind ∈ {live_console, debug_log}` with `finality = provisional` while the run is open |
| Verified final test results | `src.kind ∈ {junit_final, artifact_file}` → `finality = verified` |
| Derived metrics | Rollup tables carry `derived=true` semantics. Every rollup row stores `n` (the sample count) and the source table, so it is never shown as a raw reading. |
| Inferred failure explanations | `failure_classifications{category, confidence, rule_id, evidence}`, kept separate from outcome |
| Missing or stale information | `finality ∈ {unverified_final, inferred}`, `journal.gap`, `ingest_gaps`, `coalesced_n > 1`, liveness `stale`, and NULL metric columns (unavailable) |

`src` also carries a `parser` identifier, for example `maestro-console@2.12`. Wrong results can then be traced to a parser version and re-derived after a fix.

### 5.4 Rust type sketch (`fleet-protocol`) [P]

```rust
//! fleet-protocol: shared by agent and server. Pure data + validation, no I/O.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;

/// RFC 3339 UTC timestamp, ms precision. (Concrete time crate chosen elsewhere.)
pub type WallTime = Timestamp;

// ---------------------------------------------------------------- frames
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    Events(EventBatch),
    Telemetry(TelemetryBatch),
    Heartbeat(Heartbeat),
    UploadRequest(UploadRequest),
    UploadComplete { r#ref: u32, artifact_id: Uuid },
    Bye { reason: ByeReason },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome(Welcome),
    Reject { code: ErrorCode, message: String, retry_after_ms: Option<u64>, fatal: bool },
    Ack { through: u64, epoch: u32, #[serde(default)] duplicates: u32 },
    TelemetryAck { through: u64 },
    EventRejected { seq: u64, code: ErrorCode, permanent: bool, message: String },
    Throttle(Throttle),
    Snapshot(Snapshot),
    UploadGrant(UploadGrant),
    Goaway { reason: GoawayReason, reconnect_after_ms: u64, endpoint: Option<String> },
    Error { code: ErrorCode, fatal: bool, message: String, expected_seq: Option<u64> },
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Hello {
    pub protocol: ProtocolRange,               // {min, max}
    pub agent: AgentInfo,                      // version, git_sha, os, arch
    pub agent_run_id: Uuid,                    // UUIDv7, once per process
    pub previous_agent_run_id: Option<Uuid>,   // restart within same job
    pub ci: CiContext,                         // claimed; server prefers OIDC claims
    pub resume: ResumeState,
    pub clock: ClockSample,
    #[serde(default)] pub capabilities: Vec<String>, // "telemetry.coalesce", "upload.presigned", "spool"
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ResumeState {
    pub next_seq: u64,               // next seq the agent will assign
    pub oldest_unacked_seq: Option<u64>,
    pub telemetry_next: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClockSample {
    pub wall: WallTime,   // agent UTC wall clock at send
    pub mono_ms: u64,     // ms since agent process start, monotonic clock
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CiContext {
    pub provider: CiProvider,                 // github_actions | generic | …
    pub repository: Option<String>,           // "org/app"
    pub repository_id: Option<String>,
    pub run_id: Option<String>,
    pub run_attempt: Option<u32>,
    pub run_number: Option<u64>,
    pub workflow: Option<String>,
    pub job_key: String,                      // §3.1 derivation
    pub job_name: Option<String>,
    pub job_url: Option<String>,
    pub git: GitContext,                      // sha, ref, branch, pr_number, default_branch?
    pub runner: RunnerContext,                // runner_name, labels, environment (hosted/self)
}

// ---------------------------------------------------------------- journal
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EventBatch { pub events: Vec<EventEnvelope> }

/// Wire envelope. `data` is kept raw so unknown (type, v) survive round-trip.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,                 // e.g. "flow.ended"
    #[serde(default = "one")]
    pub v: u16,                       // per-type payload version
    pub at: ClockSample,              // when it happened (not when sent)
    #[serde(default, skip_serializing_if = "Subject::is_empty")]
    pub subject: Subject,
    pub src: Provenance,
    #[schemars(with = "serde_json::Value")]
    pub data: Box<RawValue>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct Subject {
    pub suite_execution_id: Option<Uuid>,
    pub shard_id: Option<Uuid>,
    pub device_session_id: Option<Uuid>,
    pub flow_execution_id: Option<Uuid>,
    pub step: Option<StepRef>,
    pub artifact_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StepRef {
    pub index: u32,         // ordinal within the flow execution (top-level+nested, pre-order)
    pub path: Vec<u32>,     // index path through nested runFlow/retry/repeat blocks
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Declared,        // CI-supplied manifest / CLI args
    AgentObserved,   // agent's own direct observation (adb, signals, spawn, files)
    ProcessExit,     // Maestro exit status
    LiveConsole,     // parsed stdout/stderr
    DebugLog,        // parsed maestro.log
    ArtifactFile,    // structured per-flow artifacts (e.g. commands JSON)
    JunitFinal,      // Maestro's final JUnit report
    #[serde(other)]
    Unrecognized,
}
// ServerInferred exists only server-side; agents may not send it (rejected).

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Provenance {
    pub kind: SourceKind,
    pub parser: Option<String>,       // "maestro-console@2.12" — version of OUR parser+target
    pub artifact_id: Option<Uuid>,    // for ArtifactFile/JunitFinal
    pub locator: Option<String>,      // line no., xpath-ish, file path (sanitised)
}

// ---------------------------------------------------------------- typed bodies
/// Agent-side construction & server-side dispatch target.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data")]
pub enum EventBody {
    #[serde(rename = "run.started")]        RunStarted(RunStarted),
    #[serde(rename = "worker.identified")]  WorkerIdentified(WorkerIdentified),
    #[serde(rename = "device.discovered")]  DeviceDiscovered(DeviceDiscovered),
    #[serde(rename = "device.state_observed")] DeviceStateObserved(DeviceState),
    #[serde(rename = "suite.started")]      SuiteStarted(SuiteStarted),
    #[serde(rename = "suite.planned")]      SuitePlanned(SuitePlanned),
    #[serde(rename = "shard.assigned")]     ShardAssigned(ShardAssigned),
    #[serde(rename = "flow.started")]       FlowStarted(FlowStarted),
    #[serde(rename = "step.started")]       StepStarted(StepStarted),
    #[serde(rename = "step.ended")]         StepEnded(StepEnded),
    #[serde(rename = "flow.ended")]         FlowEnded(FlowEnded),
    #[serde(rename = "flow.reported")]      FlowReported(FlowReported),
    #[serde(rename = "report.parsed")]      ReportParsed(ReportParsed),
    #[serde(rename = "process.exited")]     ProcessExited(ProcessExited),
    #[serde(rename = "artifact.registered")] ArtifactRegistered(ArtifactRegistered),
    #[serde(rename = "run.signal")]         RunSignal(RunSignal),
    #[serde(rename = "run.finalizing")]     RunFinalizing(RunFinalizing),
    #[serde(rename = "run.ended")]          RunEnded(RunEnded),
    #[serde(rename = "agent.warning")]      AgentWarning(AgentWarning),
    #[serde(rename = "journal.gap")]        JournalGap { from_seq: u64, to_seq: u64, reason: String },
    #[serde(rename = "parser.unrecognized")] ParserUnrecognized { parser: String, line_sha256: String, count: u32 },
}

impl EventEnvelope {
    /// Server: Ok(Some(body)) known, Ok(None) unknown type/version (store, ack, don't apply).
    pub fn decode(&self) -> Result<Option<EventBody>, DecodeError> { /* match (kind, v) */ }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowOutcome {
    Passed, Failed, Skipped, Interrupted, Unknown,
    #[serde(other)] Unrecognized,   // → treated as Unknown, never Passed
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FlowStarted {
    pub flow: FlowRef,              // { path (repo-relative, normalised), name }
    pub ordinal: u32,               // 0-based execution order within the shard
    pub replica_index: Option<u32>, // shard-all: which replica (== shard index)
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FlowEnded {
    pub outcome: FlowOutcome,
    pub duration_ms: Option<u64>,   // from monotonic clock or Maestro's own figure
    pub warnings: bool,             // Maestro WARNING status
    pub failure: Option<FailureInfo>, // { message (redacted, truncated), step: Option<StepRef> }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FlowReported {         // one <testcase>, already matched by the agent (§8)
    pub testsuite_index: u32,
    pub testcase_index: u32,
    pub matched: MatchInfo,         // { method: positional|name_file|name|unmatched, confidence }
    pub junit: JunitTestcase,       // id, name, classname, file, time_s, timestamp_local, status, failure, properties
    pub outcome: FlowOutcome,       // mapped from Maestro status (§8.2)
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunEnded {
    pub exit_code_returned: i32,    // what the agent returned to CI (== Maestro's)
    pub maestro_exit: Option<ExitInfo>, // code / signal
    pub interrupted: bool,          // CI cancel observed
    pub final_for_job: Option<bool>,// user hint: no further agent runs in this job
    pub dropped: DropCounters,      // telemetry samples coalesced/dropped, journal gaps
}

// ---------------------------------------------------------------- telemetry
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TelemetryBatch {
    pub tseq_from: u64,
    pub tseq_to: u64,
    #[serde(default)] pub dropped_since_last: u32,
    pub host: Vec<HostSample>,
    pub devices: Vec<DeviceSample>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostSample {
    pub at: ClockSample,
    pub coalesced_n: u16,                // 1 = raw 5 s sample; >1 = agent-side aggregate
    pub cpu_pct: Option<f32>,            // None = unavailable (never 0 as a placeholder)
    pub logical_cores: Option<u16>,
    pub load1: Option<f32>,
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub mem_pressure: Option<MemPressure>,      // normal|warn|critical (macOS) / PSI-derived (Linux)
    pub thermal: Option<ThermalReading>,        // { state: nominal|fair|serious|critical, source }
    pub cpu_temp_c: Option<f32>,                // only from a real sensor
    pub maestro_proc: Option<ProcSample>,       // cpu_pct, rss_bytes
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DeviceSample {
    pub at: ClockSample,
    pub device_session_id: Uuid,
    pub coalesced_n: u16,
    pub emulator_proc: Option<ProcSample>,   // HOST-side process usage
    pub adb_state: Option<AdbState>,
    pub boot_completed: Option<bool>,
    pub guest: Option<GuestSample>,          // explicitly guest-side; separate struct
}
```

Notes:

* `HostSample` and `GuestSample` are different types. That enforces at compile time the spec rule "do not mix host CPU with guest CPU".
* `Option` means "unavailable". Units are part of the field names.
* Per-run *capabilities* (for example "thermal: unsupported on Linux container") are sent once in `worker.identified`. They are not repeated on every sample.

### 5.5 Identifier design [P]

* **UUIDv7 generated by the agent** for every execution entity. The agent uses `Uuid::now_v7()`, which is ordered within the process [V]. IDs are generated *before* the event is enqueued, so a replay always carries the same ID. The server needs no dedup state beyond unique keys.
* The server validates every agent-supplied UUIDv7: version must be 7, and the embedded timestamp must be within ±7 days of the server's time. That catches broken clocks and nonsense IDs. On a mismatch it rejects permanently.
* **Server-generated** (`DEFAULT uuidv7()`, PG 18 [V]) for pipeline runs, attempts, jobs, workers, flow definitions, condition sets, users, credentials and audit rows.
* **Deterministic name-based IDs** for entities first created from a report, such as testcases never seen live: `uuid_v8(sha256("fleet:junit:" ‖ suite_execution_id ‖ testsuite_index ‖ testcase_index))`. RFC 9562 discourages name-based IDs as keys only when the name may change [V]. These inputs are immutable.
* **Natural keys** are unique constraints, never primary keys. `(provider, repository_id, run_id)` identifies a pipeline run, `(+ run_attempt)` an attempt, and `(+ job_key)` a job.

### 5.6 Clock skew and time [P]

* Every event carries `at = {wall, mono_ms}`. `mono_ms` comes from the agent's monotonic clock relative to process start.
* **Durations** are always computed from `mono_ms` differences within one agent run, or taken from Maestro's own duration. They are never computed from wall-clock subtraction.
* **Offset estimate:** for each `hello`/`welcome` round trip and each heartbeat, the server computes `offset ≈ server_recv − agent_wall − rtt/2`, NTP-style. The RTT comes from a heartbeat echo carried in the next `ack`. The server keeps the sample with the lowest RTT in a sliding window and stores `agent_runs.clock_offset_ms` and `clock_rtt_ms`.
* **Storage:** keep agent wall times as sent (`occurred_at`) and add `received_at` (server clock). Data from one agent stays in one clock domain, so metric spikes and the flow timeline line up exactly. Apply the offset only when displaying data across agents, for example a Gantt chart across workers. If `|offset| > 2 s`, flag "clock skew".
* **JUnit timestamps** are local date-times with no offset, truncated to whole seconds [V]. The agent converts them using its own time zone, which is the same `ZoneId.systemDefault()` Maestro used, because both run on the same host. Treat them as *low-precision* data and prefer live `mono_ms` timing when both exist.
* **Ordering** within a run uses `seq`, never timestamps. Across runs, ordering is irrelevant to correctness, because claims merge commutatively (§7).

### 5.7 Out-of-order handling [P]

* Within an agent run, `seq` enforces order. A `flow.ended` can never be applied before its `flow.started`.
* A *terminal claim* for a flow with no earlier `flow.started` creates the flow execution. This happens when a JUnit-only flow was missed by the live parser, or when a spool replay from a predecessor run arrives after the successor's events.
* A **late `flow.started`** (lower precedence than an existing terminal claim) updates attributes such as `started_at` but cannot reopen the flow to `running`. The rule is that a terminal state can only be replaced by a higher-ranked terminal claim.
* Server-inferred claims have the lowest rank, so late agent evidence always wins.

---
## 6. State machines [P]

Conventions:

* Each entity has a **lifecycle** (where it is in time) and, once terminal, an **outcome** (what happened).
* Each outcome carries `outcome_source` (a `SourceKind`) and a **finality**: `provisional | verified | unverified_final | inferred`.
* Liveness (`online/stale/disconnected`) is *derived from timestamps at read time* and is not persisted on every tick. A background "reaper" persists only threshold crossings that have consequences, such as `detached → abandoned`.
* All transitions run inside the apply transaction (§9.4) and bump `rev`, which browser deltas use for ordering.

### 6.1 Worker liveness (spec: online, stale, disconnected, completed)

A *worker* is the machine. Its liveness is the liveness of the agent runs currently active on it.

| From | Trigger | To | Notes |
|---|---|---|---|
| (none) | `hello` accepted for any agent run on this worker | `online` | Worker upserted by `(project_id, identity_key)` |
| `online` | No inbound frame for 25 s on all active runs | `stale` | Display only. The last known values are shown with an age. |
| `stale` | Any inbound frame | `online` | |
| `online`/`stale` | Socket closed without `run.ended`, or 45 s idle timeout | `disconnected` | **Not idle** (spec §6) |
| `disconnected` | Resume (`hello` for an existing run) | `online` | |
| `online`/`stale`/`disconnected` | All runs on the worker for its current job(s) reached `ended` | `completed` | |
| `disconnected` | Resume grace expired (15 min) | `disconnected` (terminal for that job) + `lost_at` set | The worker is not deleted. A later job may bring it back to `online`. |

Ephemeral hosted runners appear as one worker per job. That is correct: they are distinct machines. Identity is `runner_name` plus a host fingerprint hash, so a reused self-hosted machine becomes the same worker.

### 6.2 Agent run ("agent session")

| From | Trigger | To | Out-of-order / notes |
|---|---|---|---|
| (none) | `hello` with new `agent_run_id` | `active` | Inserted with `epoch = 1` |
| `active` | Socket closed or idle timeout | `detached` | `detached_at = now()` |
| `detached` | `hello` resume (epoch+1) | `active` | Replay follows |
| `active` | `hello` on a new socket (takeover) | `active` (epoch+1) | Old socket fenced (§3.7) |
| `active`/`detached` | `run.finalizing` | `finalizing` | Maestro has exited. Uploads and reconciliation are in progress. |
| `finalizing`/`active` | `run.ended` committed | `ended` | Terminal for the seq space (`end_seq` stored) |
| `detached`/`finalizing` | Grace expired without `run.ended` | `abandoned` | Triggers §6.5 inference for open flows |
| `abandoned` | `hello` resume or spool upload | `active` (`late_resume = true`) | **Allowed.** Late evidence is re-applied, and inferred claims are outranked. |
| `active`/`detached`/`abandoned` | A successor run declares `previous_agent_run_id` = this run | `ended(superseded)` once the spool replay (if any) completes, otherwise `abandoned` | |

### 6.3 Job (spec: pending, starting, running, completed, failed, cancelled, interrupted, unknown)

The spec mixes lifecycle and outcome in one list. The proposal is `lifecycle ∈ {pending, starting, running, finalizing, closed}`, plus `outcome ∈ {completed, failed, cancelled, interrupted, unknown}` once the job is `closed`. The UI shows one combined label.

| From | Trigger | To | Notes |
|---|---|---|---|
| (none) | Pre-declared by a pipeline-level manifest (optional, future) | `pending` | Without provider integration, Fleet only learns of a job when an agent connects |
| (none)/`pending` | First agent run `hello` | `starting` | |
| `starting` | Any `suite.started` | `running` | |
| `running` | All agent runs of the job are in `finalizing`/`ended`/`abandoned`, at least one is `finalizing` | `finalizing` | |
| `finalizing`/`running` | All agent runs are `ended`/`abandoned` **and** (`final_for_job = true` **or** a 120 s quiet period with no new run) | `closed` | The quiet period handles several sequential `maestro-fleet run` steps in one job |
| `closed` | New agent run attaches to the same job | `running` (reopened, `rev++`, `reopened_count++`) | **Allowed.** `closed` is provisional unless `final_for_job` was given. |

Outcome on close is derived from the runs and suites:

| Condition (evaluated in order) | Outcome | Source |
|---|---|---|
| Any run observed a CI cancellation signal (`run.signal{SIGTERM/SIGINT}`) and ended | `cancelled` | `agent_observed` |
| Any run `abandoned` with no `run.ended` | `interrupted` if Maestro was running at last contact, otherwise `unknown` | `server_inferred` |
| Any suite outcome `failed`, or any Maestro exit ≠ 0 | `failed` | Best source among suites |
| All suites `passed` with exit 0 | `completed` | `process_exit` + `junit_final` |
| Otherwise | `unknown` | — |

This is the **Maestro result for the job**, not the CI job result. Spec §12 already notes the distinction. The CI job may fail in a later step that Fleet never sees.

### 6.4 Suite execution (one Maestro invocation)

| From | Trigger | To |
|---|---|---|
| (none) | `suite.started` (process spawned, args recorded with redaction) | `starting` |
| `starting` | First `flow.started`, or `suite.planned` from Maestro's plan line | `running` |
| `running`/`starting` | `process.exited` | `reporting` |
| `reporting` | `report.parsed{status}` and `suite.ended`, or the agent's report timeout | `closed` |
| `running`/`reporting` | Agent run abandoned | `closed` (inferred) |

Outcome:

* `passed`: exit 0 **and** every flow execution has outcome `passed`.
* `failed`: any flow `failed`, or exit ≠ 0 with a report.
* `interrupted`: killed by a signal the agent forwarded, or exit ≠ 0 with no report and a signal observed.
* `unknown`: no exit observed.

`report_status ∈ {complete, partial, absent, invalid}` is recorded separately.

**Never infer success:** exit 0 without a report gives suite `passed` with `outcome_source = process_exit`. Maestro returns 0 only when `passed == total` [V, TestCommand.kt]. It does **not** set individual flows to `passed` unless they have their own claims.

### 6.5 Flow execution (spec: pending, running, passed, failed, skipped, interrupted, unknown)

| From | Trigger (claim) | To | Finality | Notes |
|---|---|---|---|---|
| (none) | Planned via manifest or shard plan | `pending` | — | Only when the plan is known |
| (none)/`pending` | `flow.started` (live) | `running` | provisional | |
| `running` | `flow.ended{passed}` (live_console) | `passed` | provisional → `unverified_final` when the suite closes with no report | |
| `running` | `flow.ended{failed}` (live_console) | `failed` | provisional | |
| any | `flow.reported` (junit_final) | `passed`/`failed`/`interrupted`/`unknown` | **verified** | Overrides live claims. A disagreement sets `has_conflict` (§7). |
| (none) | `flow.reported` with no live match | Outcome from the report | verified | Out-of-order creation is allowed |
| `running` | `process.exited` or `run.signal` while the flow is open | `interrupted` | inferred (agent_observed) | Maestro died or was killed during the flow |
| `running` | Agent run `abandoned` | `unknown` | inferred (server_inferred) | Not `interrupted`: the flow may have finished |
| `pending` | Suite closed and the flow was never started, with Maestro's sequential-abort message seen | `skipped` (reason `not_executed`) | unverified_final | Maestro prints "…continueOnFailure is set to false, aborting running sequential Flows" [V, TestSuiteInteractor.kt] |
| `pending` | Suite closed and the flow was never started, with no reason | `unknown` (reason `not_observed`) | inferred | Do not force it to failed (spec §13) |
| `unknown`/`interrupted` (inferred) | Any claim of rank ≥ 30 | That claim's outcome | Per that claim | Late evidence wins |
| `passed`/`failed` (verified) | A lower-rank claim | No change | — | Recorded as evidence only |

### 6.6 Step

`running → passed | failed | warned | skipped`. The source is live or `artifact_file`. If Maestro's per-command metadata is uploaded (commands JSON in `--test-output-dir`, whose exact schema needs Phase-0 validation), it replaces live step rows by rank. A step left open when the flow ends becomes `unknown`. Steps never affect the flow outcome directly; the flow claim decides it.

### 6.7 Device (per agent run: `device_session`)

| From | Trigger | To |
|---|---|---|
| (none) | `device.discovered` | `discovered` |
| `discovered` | `device.state_observed{adb=device, boot_completed=false}` | `booting` |
| `discovered`/`booting` | `boot_completed=true` | `ready` |
| `ready` | `flow.started` on this device | `busy` |
| `busy` | `flow.ended`/`flow.reported` and no other open flow | `ready` |
| `ready`/`busy` | `adb_state ∈ {offline, absent}` | `offline`. Open flows get an `agent_observed` incident; the flow outcome is not changed directly. |
| `offline` | `adb_state = device` | `ready` |
| any | Agent run `ended` | `released` |
| any | Agent run `abandoned` | `lost` |

The device's displayed state inherits the agent's liveness. If the agent is stale, the UI shows "busy · last seen 40 s ago".

### 6.8 Timeouts summary

| Timer | Default | Effect |
|---|---|---|
| Stale | 25 s | Display only |
| Idle socket close | 45 s | Agent run `detached`, worker `disconnected` |
| Resume grace | 15 min | Agent run `abandoned`. Open flows `unknown(server_inferred)`. Job may close as `interrupted`/`unknown`. |
| Job quiet period | 120 s | Job `closed` after the last run ended (no `final_for_job`) |
| Agent report wait | 60 s after Maestro exits | Agent emits `report.parsed{status:absent}` |
| Retention of late-data acceptance | Until the journal partition is dropped (default 14 days) | — |

---

## 7. Evidence precedence: flow outcome as a lattice [P]

Each flow execution holds a **set of claims** `C = {(source, origin, outcome, seq, at)}`. The derived outcome is `resolve(C)`:

* Set union is commutative, associative and idempotent, and `resolve` is a pure function of the set. **Any delivery order and any number of duplicates therefore produce the same result.** This is a join-semilattice, the property CRDTs rely on.
* Claims are stored in `flow_claims` with `PRIMARY KEY (flow_execution_id, source, origin)`, so a duplicate insert is a no-op.

**Rank (higher wins):**

| Rank | Source | May claim |
|---|---|---|
| 50 | `junit_final` | passed, failed, interrupted (CANCELED/STOPPED), unknown |
| 40 | `artifact_file` | passed, failed |
| 30 | `live_console`, `debug_log` | passed, failed |
| 20 | `process_exit`, `agent_observed` | **interrupted, unknown only** |
| 10 | `server_inferred` | **unknown, interrupted only** |
| 0 | `declared` | pending, skipped (excluded by selection) |

```text
resolve(C):
  T := { c ∈ C | c.outcome is terminal }
  if T = ∅: return (running if ∃ flow.started else pending, provisional)
  r* := max rank in T
  W  := { c ∈ T | rank(c) = r* }
  if all outcomes in W agree: winner := that outcome
  else:                                  # same-rank disagreement (e.g. two live parsers)
      winner := latest by (agent_run order, seq); conflict := true
  conflict |= ∃ c ∈ T with rank(c) ≥ 30, c.outcome ∈ {passed,failed}, c.outcome ≠ winner
  assert not (winner = passed and r* < 30)          # enforced at claim validation, too
  finality := verified          if r* ≥ 40
              provisional       if r* = 30 and suite not closed
              unverified_final  if r* = 30 and suite closed
              inferred          if r* ≤ 20
  return (winner, source_of(W), finality, conflict)
```

**Does JUnit `passed` override a live `failed`?** Yes, with `has_conflict = true` and a `parser.disagreement` counter.

* *Rationale:* the JUnit file is Maestro's own serialisation of its in-memory `FlowResult` [V]. Live state comes from Fleet's heuristic over console text. When they disagree, the parser is the more likely culprit. A disagreement is exactly the compatibility alarm that spec §15 asks for: "changes in CLI output must not silently create false pass/fail results".
* The opposite case, live `passed` with JUnit `failed`, resolves the same way: `failed` (verified).
* An `agent_observed` device incident during a flow that JUnit says `passed` does not change the outcome. It is attached as context and can feed failure classification later.

---

## 8. Reconciliation: live flow executions ↔ JUnit test cases

### 8.1 Verified facts about Maestro's JUnit output (2.12.x, `main` as of 2026-11-03) [V]

From `JUnitTestSuiteReporter.kt`, `TestSuiteReporter.kt`, `TestCommand.kt`, `TestSuiteInteractor.kt` and `TestExecutionSummary.kt`:

* Root `<testsuites>` with one `<testsuite>` per **shard**. Each shard's `TestSuiteInteractor` produces one `SuiteResult`. After `awaitAll()` the results are merged (`mergeSummaries`) **in shard-index order**, and `saveReport()` runs **once, after all shards finish**. *Consequence:* if Maestro is killed or crashes, there is normally **no JUnit file at all**. "Partial" reports are mainly truncated or corrupt files, or multi-invocation setups.
* `<testsuite name=(--test-suite-name or "Test Suite") device=device.description tests=flows.size failures=count(status==ERROR) time timestamp>`.
* `<testcase id=(junitId property or flow name) name=flow name classname=(junitClassname or flow name) file=filePath time timestamp status>` with an optional `<failure>` (text only, no `message`/`type` attributes) and optional `<properties>` (custom flow properties and `tags`).
* `status` is a **non-standard attribute** taking Maestro `FlowStatus` values: `PENDING, PREPARING, INSTALLING, RUNNING, SUCCESS, ERROR, CANCELED, STOPPED, WARNING`.
* **There is no `<skipped/>` element.** `failures` counts only `ERROR`.
* Failure messages are prefixed with `"[shard N] "` when sharded.
* `filePath` is `toCwdRelativeOrAbsoluteString()`: relative to Maestro's CWD when possible, otherwise absolute. The agent must normalise it to a repo-relative path using the repo root (`GITHUB_WORKSPACE` or `git rev-parse --show-toplevel`).
* `timestamp` is an ISO local date-time with no offset, truncated to seconds, in the Maestro host's time zone. `time` is in seconds.
* Test cases within a suite follow **execution order**: sequential-config flows first, then the rest, in the order of `flowResults.add`.
* Sharding:
  * `--shard-split N` distributes flows round-robin (`index % shards`).
  * `--shard-all N` runs `plan.copy()` on every shard.
  * Maestro prints "Will split $flowCount flows across $effectiveShards shards…" or "Will run $effectiveShards shards, with all $flowCount flows in each shard". These lines are a `live_console` source for **planned totals**; confirm the exact wording in Phase 0.
  * Per-flow debug directories get a `-shard-N` suffix, plus hash and collision suffixes for long or duplicate names.
* With a single shard, `shardIndex` is null: there is no prefix and no suffix.

### 8.2 Status mapping [P]

| Maestro `status` | Fleet outcome | Notes |
|---|---|---|
| SUCCESS | passed | |
| WARNING | passed (`warnings = true`) | Confirm WARNING semantics in Phase 0 |
| ERROR | failed | |
| CANCELED, STOPPED | interrupted | |
| PENDING, PREPARING, INSTALLING, RUNNING | unknown (`reason: non_terminal_in_report`) | Cloud-oriented states. Not expected locally. |
| missing or unrecognised | unknown | Never passed |

### 8.3 Algorithm (shared Rust crate, run by the agent; the server can re-run it from stored data) [P]

The spec assigns reconciliation to the agent. The proposal is to put it in a pure library inside `fleet-protocol` (or `fleet-reconcile`), used by:

* the agent at the end of the run,
* `maestro-fleet inspect` offline,
* the server when re-reconciling after an algorithm fix, using the uploaded JUnit artifact and the stored live claims.

Output is a set of `flow.reported` claims with deterministic IDs, so re-running is idempotent.

```text
input:
  S        : suite execution (shard_mode ∈ {none, split, all}, shards[0..k) each with device_session + device description)
  L[s]     : live flow executions of shard s, ordered by ordinal (each: id, path_norm, name, live_outcome)
  R        : parsed report = list of testsuites TS[0..m), each with testcases TC[0..n_i) in file order
  report_status ∈ {complete, partial (salvaged by streaming parser), invalid, absent}

if report_status ∈ {absent, invalid}:
    emit report.parsed{status}; for each live flow: finality := unverified_final (by resolve); return

# 1. Map testsuites → shards
if m == k:
    if all i: TS[i].device ≈ shards[i].device_description  → map i→i            (method=positional)
    elif bijection by device description exists           → map by description (method=device)
    else                                                   → map i→i, confidence=low, warn
elif k == 1 and m == 1: map 0→0
else:
    map by device description where unique; unmapped testsuites → shard=None (suite-level), warn

# 2. Align testcases with live flows inside each mapped shard
for each (TS[i] → shard s):
    A := align(L[s], TS[i].testcases)
    for (live?, tc) in A:
        key := (S.id, i, tc.position)
        id  := live?.id  if live? present  else uuid_v8(key)          # deterministic
        emit flow.reported{flow_execution_id=id, testsuite_index=i, testcase_index=tc.position,
                           matched=method/confidence, junit=tc, outcome=map(tc.status)}
    for live flows not in A: keep; tag reason "absent_from_report"   # never forced to failed

# 3. Suite-level (unmapped) testcases: match globally by (path_norm, name), occurrence order; else create

# 4. Consistency checks → agent.warning / parser.disagreement
    TS[i].tests == len(TS[i].testcases); TS[i].failures == count(ERROR)
    count(live flows of shard s) vs len(TS[i].testcases)
    shard_mode=all ⇒ every TS has the same multiset of (path,name)
    planned totals (if known) vs report totals

align(live L, cases T):        # order-preserving, duplicate-name-safe
    norm(x) := (normalize_path(x.file or x.path), x.name)
    # Fast path: same length and pairwise equal norm → zip (method=positional, confidence=high)
    if |L| == |T| and ∀j: norm(L[j]) == norm(T[j]): return zip(L, T)
    # General path: LCS / Needleman–Wunsch on norm with cost:
    #   match equal norm: 0; match equal name only (file missing/moved): 1;
    #   skip live (parser hallucinated or report truncated): 2; skip case (parser missed): 2
    # Ties broken by smaller |ordinal - position|. Equal names are matched k-th to k-th
    # occurrence automatically because alignment is order-preserving.
    return path with minimum cost, each pair tagged with method & confidence
```

How the algorithm covers each case:

* **Duplicate flow names:** alignment preserves order, and Maestro writes test cases in execution order. Duplicates pair k-th with k-th.
* **Shard-all:** every shard has its own testsuite, so each device gives a separate flow execution (spec §3). Nothing is merged across devices.
* **Partial report:** a streaming parser salvages complete `<testcase>` elements before the point of truncation, and `report_status = partial`. Shards or test cases past the truncation keep their live outcomes.
* **Absent report:** live outcomes stand with `finality = unverified_final`. The UI shows "not verified by final report".
* **Flows seen only in the report:** created with deterministic IDs and `finality = verified`, with no live timing.

---
## 9. PostgreSQL schema (PG 18 baseline)

### 9.1 Design decisions [P]

| Decision | Choice | Justification |
|---|---|---|
| PG version | **Require ≥ 18**. Test against 19 after GA (planned 2026-11-19). | Needs `uuidv7()` for server defaults [V]. Uses OLD/NEW `RETURNING` to detect real changes for broadcasts [V]. PG 14 reaches EOL on 2026-11-12 [V]. PG 19's `ON CONFLICT DO SELECT` is optional and goes behind a version check. |
| Primary keys | `uuid`. Agent-generated v7 for execution entities. `DEFAULT uuidv7()` for server entities. | Time-ordered IDs give B-tree locality, which RFC 9562 says can be "one order of magnitude or more" better [V]. Agent IDs give idempotency. |
| Raw journal vs only derived state | **Both.** `agent_events` is append-only and partitioned by `run_day`, kept 14 days by default. Derived tables are long-lived. They are written in **the same transaction**. | The journal lets you (1) re-derive state after a parser, reconciliation or precedence fix, (2) debug "why does Fleet say X" by showing the raw evidence, (3) audit what an agent sent. A journal alone (pure event sourcing) would make analytics queries and retention painful. Derived state alone would lose evidence and make reprocessing impossible. |
| Enums | `text` plus `CHECK`, through domains | Easier to evolve than `CREATE TYPE … AS ENUM`: you can add and remove values with one constraint swap. |
| JSONB | Raw journal `data`, `flow_claims.detail`, `condition_sets.dims`, `*.extra`, capabilities, policy | Typed columns for anything filtered, grouped or joined. JSONB for evidence detail and forward-compat leftovers. Promote a JSONB field to a column by migration once it is queried. |
| `project_id` on every row | Denormalised | Scoping becomes trivial and indexable. Optional RLS later. Prevents cross-tenant joins by mistake. |
| Counters | **No** hot counters on `jobs` or `pipelines` during a run | Concurrent agent runs would contend on one row. Live counts come from the in-memory projection or from `count(*)` over the indexed `flow_executions`. Counts are persisted at close. |
| FKs | Normal FKs among operational tables. **No FKs into or out of** `agent_events` or the telemetry tables. `step_executions` → `flow_executions` is kept. | Partitioned log tables must stay cheap to insert into and drop. PG 18 allows `NOT VALID` FKs on partitioned tables if you want them [V]. |
| Partition management | Application-managed: create `today−1 … today+7`, drop past retention | pg_partman 5.x is fine where it is available [V], but it needs an extension in the image. The spec wants "operationally small". The application code is about 150 lines. |

### 9.2 DDL sketch [P]

```sql
-- ============================================================ conventions
CREATE DOMAIN outcome_t  AS text CHECK (VALUE IN ('passed','failed','skipped','interrupted','unknown'));
CREATE DOMAIN finality_t AS text CHECK (VALUE IN ('provisional','verified','unverified_final','inferred'));
CREATE DOMAIN source_t   AS text CHECK (VALUE IN ('declared','agent_observed','process_exit','live_console',
                                                  'debug_log','artifact_file','junit_final','server_inferred'));
CREATE DOMAIN sha256_t   AS bytea CHECK (octet_length(VALUE) = 32);

-- ============================================================ tenancy & auth (§13)
CREATE TABLE projects (
  id           uuid PRIMARY KEY DEFAULT uuidv7(),
  slug         text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
  name         text NOT NULL,
  default_branch text NOT NULL DEFAULT 'main',
  settings     jsonb NOT NULL DEFAULT '{}',          -- policy: timeouts, artifact allowlist, metric interval
  created_at   timestamptz NOT NULL DEFAULT now(),
  archived_at  timestamptz
);

CREATE TABLE users (
  id           uuid PRIMARY KEY DEFAULT uuidv7(),
  oidc_issuer  text NOT NULL,
  oidc_subject text NOT NULL,
  email        text,
  display_name text,
  is_admin     boolean NOT NULL DEFAULT false,       -- platform administrator
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_login_at timestamptz,
  UNIQUE (oidc_issuer, oidc_subject)
);

CREATE TABLE role_bindings (
  id             uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id     uuid REFERENCES projects,           -- NULL = all projects
  principal_kind text NOT NULL CHECK (principal_kind IN ('user','oidc_group')),
  principal      text NOT NULL,                      -- users.id::text or group claim value
  role           text NOT NULL CHECK (role IN ('viewer','maintainer','admin')),
  created_by     uuid REFERENCES users,
  created_at     timestamptz NOT NULL DEFAULT now(),
  UNIQUE NULLS NOT DISTINCT (project_id, principal_kind, principal)
);

CREATE TABLE publisher_credentials (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id    uuid NOT NULL REFERENCES projects,
  name          text NOT NULL,
  kind          text NOT NULL CHECK (kind IN ('static_token','oidc_trust')),
  -- static_token: only a hash of a 256-bit random token is stored; prefix aids identification
  token_prefix  text,
  token_hash    sha256_t,
  -- oidc_trust: e.g. GitHub Actions issuer + audience + claim conditions
  oidc_issuer   text,
  oidc_audience text,
  claim_rules   jsonb,          -- {"repository_id":"123","ref":{"glob":"refs/heads/*"},"event_name":{"not":"pull_request_target"}}
  allow_untrusted_prs boolean NOT NULL DEFAULT false,
  expires_at    timestamptz,
  revoked_at    timestamptz,
  created_by    uuid REFERENCES users,
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_used_at  timestamptz,
  CHECK ((kind = 'static_token') = (token_hash IS NOT NULL)),
  CHECK ((kind = 'oidc_trust')  = (oidc_issuer IS NOT NULL))
);
CREATE UNIQUE INDEX ON publisher_credentials (token_hash) WHERE token_hash IS NOT NULL;

CREATE TABLE audit_events (                 -- append-only; app role has INSERT/SELECT only
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  at          timestamptz NOT NULL DEFAULT now(),
  actor_kind  text NOT NULL CHECK (actor_kind IN ('user','publisher','system')),
  actor_id    uuid,
  project_id  uuid,
  action      text NOT NULL,                 -- 'credential.rotate', 'retention.update', 'artifact.download', ...
  target_kind text, target_id uuid,
  details     jsonb NOT NULL DEFAULT '{}',
  ip          inet
);
CREATE INDEX ON audit_events (project_id, at DESC);

-- ============================================================ CI hierarchy (§3)
CREATE TABLE pipeline_runs (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id    uuid NOT NULL REFERENCES projects,
  provider      text NOT NULL,                       -- 'github_actions' | 'generic' | ...
  provider_run_key text NOT NULL,                    -- github: "<repository_id>:<run_id>"
  repository    text, workflow text, run_number bigint, event text,
  branch        text, commit_sha text, pr_number integer, url text,
  is_default_branch boolean NOT NULL DEFAULT false,
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, provider, provider_run_key)
);
CREATE INDEX ON pipeline_runs (project_id, created_at DESC);
CREATE INDEX ON pipeline_runs (project_id, branch, created_at DESC);

CREATE TABLE pipeline_attempts (
  id              uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id      uuid NOT NULL,
  pipeline_run_id uuid NOT NULL REFERENCES pipeline_runs,
  attempt         integer NOT NULL CHECK (attempt >= 1),
  first_seen_at   timestamptz NOT NULL DEFAULT now(),
  ci_result       text,                              -- only from a future provider integration
  UNIQUE (pipeline_run_id, attempt)
);

CREATE TABLE jobs (
  id                  uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id          uuid NOT NULL,
  pipeline_attempt_id uuid NOT NULL REFERENCES pipeline_attempts,
  job_key             text NOT NULL,                 -- §3.1
  name                text, url text,
  lifecycle           text NOT NULL DEFAULT 'starting'
                      CHECK (lifecycle IN ('pending','starting','running','finalizing','closed')),
  outcome             text CHECK (outcome IN ('completed','failed','cancelled','interrupted','unknown')),
  outcome_source      source_t,
  reopened_count      integer NOT NULL DEFAULT 0,
  counts              jsonb,                          -- persisted at close: planned/started/passed/...
  started_at          timestamptz, closed_at timestamptz,
  rev                 bigint NOT NULL DEFAULT 0,
  UNIQUE (pipeline_attempt_id, job_key)
);
CREATE INDEX ON jobs (project_id, lifecycle) WHERE lifecycle <> 'closed';   -- "what's running now"

CREATE TABLE workers (
  id             uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id     uuid NOT NULL REFERENCES projects,
  identity_key   text NOT NULL,                      -- runner_name + sha256(host fingerprint)
  runner_name    text, os text, arch text, cpu_model text,
  logical_cores  smallint, mem_total_bytes bigint,
  capabilities   jsonb NOT NULL DEFAULT '{}',        -- {"thermal":"unsupported:linux-container", ...}
  first_seen_at  timestamptz NOT NULL DEFAULT now(),
  last_seen_at   timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, identity_key)
);

CREATE TABLE agent_runs (                             -- spec: agent_sessions + ingestion_cursors
  id                    uuid PRIMARY KEY,             -- agent-generated UUIDv7
  project_id            uuid NOT NULL REFERENCES projects,
  job_id                uuid NOT NULL REFERENCES jobs,
  worker_id             uuid REFERENCES workers,
  credential_id         uuid REFERENCES publisher_credentials,
  previous_agent_run_id uuid REFERENCES agent_runs,
  run_day               date NOT NULL,                -- server date at first hello; partition key for logs
  agent_version         text NOT NULL,
  protocol_version      smallint NOT NULL,
  state                 text NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active','detached','finalizing','ended','abandoned')),
  end_reason            text,                         -- 'normal','superseded','interrupted',...
  late_resume           boolean NOT NULL DEFAULT false,
  epoch                 integer NOT NULL DEFAULT 0,   -- connection fencing (§3.7)
  committed_seq         bigint  NOT NULL DEFAULT 0,   -- journal cursor
  end_seq               bigint,
  telemetry_committed   bigint  NOT NULL DEFAULT 0,
  clock_offset_ms       integer, clock_rtt_ms integer,
  maestro_exit_code     integer, maestro_exit_signal text,
  started_at            timestamptz NOT NULL DEFAULT now(),
  last_frame_at         timestamptz NOT NULL DEFAULT now(),
  detached_at           timestamptz, ended_at timestamptz
);
CREATE INDEX ON agent_runs (job_id);
CREATE INDEX ON agent_runs (state, last_frame_at) WHERE state IN ('active','detached','finalizing'); -- reaper

CREATE TABLE ingest_gaps (
  agent_run_id uuid NOT NULL REFERENCES agent_runs,
  stream       text NOT NULL CHECK (stream IN ('journal','telemetry')),
  from_seq     bigint NOT NULL, to_seq bigint NOT NULL,
  reason       text NOT NULL,                         -- 'agent_discarded','journal.gap','spool_lost'
  recorded_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (agent_run_id, stream, from_seq)
);

CREATE TABLE ingest_rejections (
  agent_run_id uuid NOT NULL, seq bigint NOT NULL,
  code text NOT NULL, message text, raw jsonb,
  rejected_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (agent_run_id, seq)
);

-- ============================================================ devices & conditions
CREATE TABLE device_profiles (                         -- the comparable "kind" of device
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  platform    text NOT NULL CHECK (platform IN ('android','ios')),
  kind        text NOT NULL CHECK (kind IN ('emulator','simulator','physical')),
  model       text, avd_or_runtime text, os_version text, api_level smallint, abi text,
  fingerprint sha256_t NOT NULL UNIQUE,                -- JCS(dims) hash
  extra       jsonb NOT NULL DEFAULT '{}'
);

CREATE TABLE device_sessions (                          -- one device as seen by one agent run
  id            uuid PRIMARY KEY,                       -- agent-generated
  project_id    uuid NOT NULL,
  agent_run_id  uuid NOT NULL REFERENCES agent_runs,
  profile_id    uuid REFERENCES device_profiles,
  serial        text NOT NULL,                          -- 'emulator-5554': attribute, NOT identity
  description   text,                                   -- Maestro device.description (JUnit @device)
  state         text NOT NULL DEFAULT 'discovered'
                CHECK (state IN ('discovered','booting','ready','busy','offline','released','lost')),
  first_seen_at timestamptz NOT NULL, last_seen_at timestamptz NOT NULL,
  rev           bigint NOT NULL DEFAULT 0
);
CREATE INDEX ON device_sessions (agent_run_id);

CREATE TABLE condition_sets (                            -- execution-condition fingerprint (§11.1)
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id  uuid NOT NULL REFERENCES projects,
  fingerprint sha256_t NOT NULL,
  dims        jsonb NOT NULL,                            -- {"device_profile":..,"app":..,"maestro":"2.12",..}
  UNIQUE (project_id, fingerprint)
);

-- ============================================================ execution
CREATE TABLE suite_executions (
  id            uuid PRIMARY KEY,                         -- agent-generated
  project_id    uuid NOT NULL,
  agent_run_id  uuid NOT NULL REFERENCES agent_runs,
  job_id        uuid NOT NULL REFERENCES jobs,
  suite_name    text,
  shard_mode    text NOT NULL CHECK (shard_mode IN ('none','split','all')),
  shard_count   smallint NOT NULL DEFAULT 1,
  maestro_version text,
  planned_count integer,                                  -- NULL = plan unknown → indeterminate progress
  plan_source   source_t,
  lifecycle     text NOT NULL CHECK (lifecycle IN ('starting','running','reporting','closed')),
  outcome       text CHECK (outcome IN ('passed','failed','interrupted','unknown')),
  outcome_source source_t,
  report_status text CHECK (report_status IN ('complete','partial','absent','invalid')),
  exit_code     integer,
  started_at    timestamptz, ended_at timestamptz,
  rev           bigint NOT NULL DEFAULT 0
);
CREATE INDEX ON suite_executions (job_id);

CREATE TABLE shards (
  id                 uuid PRIMARY KEY,                    -- agent-generated
  suite_execution_id uuid NOT NULL REFERENCES suite_executions,
  shard_index        smallint NOT NULL,
  device_session_id  uuid REFERENCES device_sessions,
  planned_count      integer,
  UNIQUE (suite_execution_id, shard_index)
);

CREATE TABLE flow_definitions (
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id  uuid NOT NULL REFERENCES projects,
  repo_path   text NOT NULL,                               -- normalised repo-relative, '/' separators
  last_name   text,                                        -- display only; never a key
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, repo_path)
);

CREATE TABLE flow_executions (
  id                  uuid PRIMARY KEY,                     -- agent-generated v7 or deterministic v8
  project_id          uuid NOT NULL,
  suite_execution_id  uuid NOT NULL REFERENCES suite_executions,
  shard_id            uuid REFERENCES shards,
  device_session_id   uuid REFERENCES device_sessions,
  flow_definition_id  uuid NOT NULL REFERENCES flow_definitions,
  condition_set_id    uuid REFERENCES condition_sets,
  ordinal             integer,                              -- execution order within shard
  replica_index       smallint,                             -- shard-all replica
  display_name        text,
  state               text NOT NULL CHECK (state IN ('pending','running','passed','failed',
                                                     'skipped','interrupted','unknown')),
  outcome_source      source_t,
  finality            finality_t NOT NULL DEFAULT 'provisional',
  has_conflict        boolean NOT NULL DEFAULT false,
  warnings            boolean NOT NULL DEFAULT false,
  failure_message     text,                                 -- redacted, truncated
  failed_step_index   integer,
  started_at          timestamptz, ended_at timestamptz,    -- agent clock domain
  duration_ms         integer,
  -- denormalised for analytics (immutable after insert):
  pipeline_attempt_id uuid NOT NULL,
  job_id              uuid NOT NULL,
  branch              text, commit_sha text, is_default_branch boolean NOT NULL DEFAULT false,
  rev                 bigint NOT NULL DEFAULT 0
  -- deliberately NOT UNIQUE(shard_id, ordinal): a report-only flow (missed by the live parser)
  -- may share an ordinal with a live one; the JUnit position lives in the junit_final claim.
);
CREATE INDEX ON flow_executions (shard_id, ordinal);
CREATE INDEX ON flow_executions (suite_execution_id);
CREATE INDEX ON flow_executions (project_id, flow_definition_id, condition_set_id, ended_at DESC)
  INCLUDE (state, duration_ms, commit_sha, is_default_branch)
  WHERE finality <> 'provisional';                           -- history, flakiness, baselines
CREATE INDEX ON flow_executions (project_id, ended_at DESC) WHERE state = 'failed';  -- recent failures
CREATE INDEX ON flow_executions (pipeline_attempt_id);

CREATE TABLE flow_claims (                                   -- evidence; outcome derived (§7)
  flow_execution_id uuid NOT NULL REFERENCES flow_executions,
  source            source_t NOT NULL,
  origin            text NOT NULL,          -- '<agent_run_id>:<seq>' or 'junit:<sha256>:<suite>:<case>' or 'rule:<id>'
  outcome           text NOT NULL CHECK (outcome IN ('pending','running','passed','failed','skipped','interrupted','unknown')),
  observed_at       timestamptz,
  detail            jsonb NOT NULL DEFAULT '{}',             -- failure text, junit attrs, parser id, match info
  recorded_at       timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (flow_execution_id, source, origin),
  CHECK (NOT (outcome = 'passed' AND source IN ('process_exit','agent_observed','server_inferred','declared')))
);

CREATE TABLE failure_classifications (                       -- spec §9: category + evidence + confidence
  flow_execution_id uuid NOT NULL REFERENCES flow_executions,
  rule_id           text NOT NULL,                           -- versioned deterministic rule
  category          text NOT NULL,                           -- assertion_failure, element_not_found, ...
  confidence        real NOT NULL CHECK (confidence BETWEEN 0 AND 1),
  evidence          jsonb NOT NULL,                          -- refs to claims, artifacts, log excerpts
  classified_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (flow_execution_id, rule_id)
);

-- Steps: high volume → partitioned by run_day (same day as the owning agent run)
CREATE TABLE step_executions (
  run_day           date NOT NULL,
  flow_execution_id uuid NOT NULL,
  step_index        integer NOT NULL,
  step_path         integer[] NOT NULL,                      -- nesting path
  command           text NOT NULL,                           -- 'assertVisible', 'runFlow', ...
  label             text,                                    -- redacted description
  status            text NOT NULL CHECK (status IN ('running','passed','failed','warned','skipped','unknown')),
  source            source_t NOT NULL,
  source_rank       smallint NOT NULL,
  started_at        timestamptz, duration_ms integer,
  error             text,
  PRIMARY KEY (run_day, flow_execution_id, step_index)
) PARTITION BY RANGE (run_day);

-- Raw journal
CREATE TABLE agent_events (
  run_day           date NOT NULL,
  agent_run_id      uuid NOT NULL,
  seq               bigint NOT NULL,
  type              text NOT NULL,
  v                 smallint NOT NULL,
  src_kind          text NOT NULL,
  occurred_at       timestamptz NOT NULL,                    -- agent wall clock
  mono_ms           bigint NOT NULL,
  received_at       timestamptz NOT NULL DEFAULT now(),
  flow_execution_id uuid,                                    -- lifted from subject for debugging lookups
  apply_status      text NOT NULL CHECK (apply_status IN ('applied','unsupported','rejected','duplicate_entity')),
  data              jsonb NOT NULL,
  PRIMARY KEY (run_day, agent_run_id, seq)                   -- globally unique: run_day fixed per run
) PARTITION BY RANGE (run_day);
-- per-partition (created by partition manager):
--   CREATE INDEX ON agent_events_pYYYYMMDD (flow_execution_id) WHERE flow_execution_id IS NOT NULL;

-- ============================================================ artifacts (§12)
CREATE TABLE blobs (
  project_id    uuid NOT NULL REFERENCES projects,
  sha256        sha256_t NOT NULL,
  size_bytes    bigint NOT NULL CHECK (size_bytes >= 0),
  storage_key   text NOT NULL,
  state         text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','present','deleting')),
  created_at    timestamptz NOT NULL DEFAULT now(),
  verified_at   timestamptz,
  unreferenced_since timestamptz,                             -- set by GC scan; cleared on new reference
  PRIMARY KEY (project_id, sha256)
);
CREATE INDEX ON blobs (unreferenced_since) WHERE unreferenced_since IS NOT NULL;

CREATE TABLE retention_classes (
  project_id uuid NOT NULL REFERENCES projects,
  class      text NOT NULL,       -- 'report','failure_evidence','success_evidence','debug_log','video','bundle'
  ttl        interval,            -- NULL = keep until project deletion
  PRIMARY KEY (project_id, class)
);

CREATE TABLE artifacts (
  id                 uuid PRIMARY KEY,                        -- agent-generated
  project_id         uuid NOT NULL,
  agent_run_id       uuid NOT NULL REFERENCES agent_runs,
  job_id             uuid NOT NULL REFERENCES jobs,
  suite_execution_id uuid REFERENCES suite_executions,
  flow_execution_id  uuid REFERENCES flow_executions,
  step_index         integer,
  kind               text NOT NULL CHECK (kind IN ('junit_xml','maestro_log','screenshot','recording',
                                                   'commands_json','html_report','debug_bundle','console_excerpt','other')),
  content_type       text NOT NULL,
  size_bytes         bigint NOT NULL,
  sha256             sha256_t NOT NULL,
  original_name      text NOT NULL,                           -- sanitised basename
  logical_path       text NOT NULL,                           -- for exports: <job>/<suite>/<shard>/<flow>/<file>
  retention_class    text NOT NULL,
  expires_at         timestamptz,
  legal_hold         boolean NOT NULL DEFAULT false,
  state              text NOT NULL CHECK (state IN ('declared','uploading','available','rejected','expired')),
  reject_reason      text,
  created_at         timestamptz NOT NULL,
  available_at       timestamptz,
  FOREIGN KEY (project_id, sha256) REFERENCES blobs (project_id, sha256)
);
CREATE INDEX ON artifacts (flow_execution_id);
CREATE INDEX ON artifacts (job_id);
CREATE INDEX ON artifacts (project_id, sha256);                -- GC reference check
CREATE INDEX ON artifacts (expires_at) WHERE state = 'available' AND NOT legal_hold;
```

**Mapping to the spec's §7 table list:**

| Spec table | Here |
|---|---|
| `projects` | `projects` |
| `pipelines` | `pipeline_runs` + `pipeline_attempts` |
| `jobs` | `jobs` |
| `workers` | `workers` |
| `devices` | `device_profiles` + `device_sessions` |
| `suite_executions` | `suite_executions` |
| `shards` | `shards` |
| `flow_definitions` | `flow_definitions` + `condition_sets` |
| `flow_executions` | `flow_executions` + `flow_claims` |
| `flow_attempts` | **View** (below) |
| `step_events` | `step_executions` (one row per step, merged start and end) + journal |
| `telemetry_samples` | `host_samples`, `device_samples` + rollups (§10) |
| `artifacts` | `artifacts` + `blobs` + `retention_classes` |
| `agent_sessions` / `ingestion_cursors` | `agent_runs` (cursor columns) + `ingest_gaps` + `ingest_rejections` |
| `audit_events` | `audit_events` |

**Added:** `users`, `role_bindings`, `publisher_credentials`, `failure_classifications`, `agent_events`.

```sql
-- "Attempt" = the k-th execution of the same flow under the same conditions for the same CI job
-- across re-runs (run_attempt) and repeated invocations. Derived, not stored.
CREATE VIEW flow_attempt_groups AS
SELECT fe.id AS flow_execution_id,
       pr.id AS pipeline_run_id, j.job_key, fe.flow_definition_id, fe.condition_set_id,
       coalesce(fe.replica_index, 0) AS replica_index,
       row_number() OVER (PARTITION BY pr.id, j.job_key, fe.flow_definition_id, fe.condition_set_id,
                                       coalesce(fe.replica_index, 0)
                          ORDER BY pa.attempt, fe.started_at NULLS LAST, fe.id) AS attempt_no
FROM flow_executions fe
JOIN jobs j               ON j.id = fe.job_id
JOIN pipeline_attempts pa ON pa.id = j.pipeline_attempt_id
JOIN pipeline_runs pr     ON pr.id = pa.pipeline_run_id;
```

### 9.3 Idempotent upsert patterns [P]

```sql
-- (a) Natural-key entity, returns id whether inserted or existing (PG 18)
INSERT INTO jobs (project_id, pipeline_attempt_id, job_key, name, url)
VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (pipeline_attempt_id, job_key) DO UPDATE
  SET name = coalesce(jobs.name, EXCLUDED.name)        -- harmless write so RETURNING yields the row
RETURNING id;
-- PG 19 alternative (no write): ... ON CONFLICT (pipeline_attempt_id, job_key) DO SELECT RETURNING id;  [V]

-- (b) Agent-generated entity with ownership check
INSERT INTO flow_executions (id, project_id, suite_execution_id, ..., state)
VALUES ($id, $project, $suite, ..., 'running')
ON CONFLICT (id) DO NOTHING;
-- then: SELECT project_id, suite_execution_id FROM flow_executions WHERE id = $id
-- if it differs from the caller's → permanent reject 'foreign_entity' + audit_events row.

-- (c) Claim + re-derive, broadcasting only on real change (PG 18 OLD/NEW RETURNING) [V]
INSERT INTO flow_claims (flow_execution_id, source, origin, outcome, observed_at, detail)
VALUES ($fe, $src, $origin, $outcome, $at, $detail)
ON CONFLICT DO NOTHING;

UPDATE flow_executions f
   SET state = r.state, outcome_source = r.source, finality = r.finality,
       has_conflict = r.conflict, rev = f.rev + 1
  FROM resolve_flow($fe) r                                -- SQL or app-side (§7)
 WHERE f.id = $fe
   AND (f.state, f.outcome_source, f.finality, f.has_conflict)
       IS DISTINCT FROM (r.state, r.source, r.finality, r.conflict)
RETURNING old.state AS old_state, new.state AS new_state, new.rev;

-- (d) Rank-guarded step upsert: artifact-derived steps replace live ones, never the reverse
INSERT INTO step_executions AS s (run_day, flow_execution_id, step_index, ..., source, source_rank)
VALUES (...)
ON CONFLICT (run_day, flow_execution_id, step_index) DO UPDATE
   SET status = EXCLUDED.status, duration_ms = EXCLUDED.duration_ms, error = EXCLUDED.error,
       source = EXCLUDED.source, source_rank = EXCLUDED.source_rank
 WHERE EXCLUDED.source_rank >= s.source_rank;
```

### 9.4 Apply transaction (per `events` frame) [P]

```text
BEGIN
  -- 1. fence + cursor (row lock serialises with any zombie)
  SELECT committed_seq, epoch, state, project_id, run_day FROM agent_runs WHERE id=$run FOR UPDATE
  if epoch ≠ my_epoch → ROLLBACK; send error{superseded}; close
  batch := events with seq > committed_seq          (duplicates counted, skipped)
  require batch.first.seq = committed_seq+1 and contiguous, else ROLLBACK; error{seq_gap}
  -- 2. journal (one statement)
  INSERT INTO agent_events (...) SELECT * FROM unnest($run_day[], $run[], $seq[], ..., $data::jsonb[])
  -- 3. derived state, per event, in seq order (in-memory decode; statements batched where possible)
  for e in batch: apply(e)    -- upserts (§9.3); unknown type ⇒ apply_status='unsupported'; invalid ⇒ ingest_rejections
  -- 4. advance cursor
  UPDATE agent_runs SET committed_seq=$last, last_frame_at=now() WHERE id=$run AND epoch=$my_epoch
COMMIT
→ send ack{through:$last}; publish changed entities (rev) to live hub
```

* **Effectively-once in the database:** the cursor and the effects commit atomically. Agent-generated IDs and claim primary keys protect paths that skip the cursor, such as spool uploads and re-reconciliation.
* **Isolation:** READ COMMITTED is enough. The `FOR UPDATE` on `agent_runs` serialises work per run. Cross-run writes to a shared job touch only upserts with unique keys, which are safe under concurrency.

---
## 10. Telemetry storage without TimescaleDB

### 10.1 Layout [P]

| Table | Grain | Partitioning | Retention (default) | Primary key / indexes |
|---|---|---|---|---|
| `host_samples` | One row per agent run per 5 s (wide, typed columns) | `RANGE (ts)`, **daily** | 7 days (raw, for recent failure investigation) | `PK (agent_run_id, ts)`, which dedups replays. `BRIN (ts)` per partition. |
| `device_samples` | One row per device session per 5 s | `RANGE (ts)`, daily | 7 days | `PK (device_session_id, ts)`, `BRIN (ts)` |
| `host_metrics_1m`, `device_metrics_1m` | 1-minute aggregates (`n`, avg, max, min, last per metric) | `RANGE (bucket)`, **monthly** | 90 days (spec §7) | `PK (agent_run_id \| device_session_id, bucket)`, plus `(worker_id, bucket)` |
| `host_metrics_1h`, `worker_utilization_daily` | Hourly and daily aggregates | Yearly or unpartitioned | 1 year (spec "aggregate statistics") | Small |
| `flow_daily_stats` | Per (project, flow def, condition, default-branch flag, day): runs, passed, failed, interrupted, unknown, duration histogram `int4[]` | Unpartitioned | 1 year+ | `PK (project_id, flow_definition_id, condition_set_id, is_default_branch, day)` |

**Wide rows rather than one row per metric.** The metric set is fixed and known. A wide row costs one tuple header (about 24 B) per sample instead of one per metric, which is roughly 10× fewer rows (§10.3). Units are encoded in column names (`cpu_pct`, `mem_available_bytes`). NULL means unavailable, which meets spec §16 #5: never a fake value. `coalesced_n > 1` marks agent-side aggregates.

```sql
CREATE TABLE host_samples (
  ts                  timestamptz NOT NULL,          -- agent clock domain (offset kept on agent_runs)
  agent_run_id        uuid NOT NULL,
  worker_id           uuid NOT NULL,
  project_id          uuid NOT NULL,
  coalesced_n         smallint NOT NULL DEFAULT 1,
  cpu_pct             real,  load1 real,
  mem_total_bytes     bigint, mem_available_bytes bigint, swap_used_bytes bigint,
  mem_pressure        smallint,                      -- 0 normal,1 warn,2 critical; NULL unavailable
  thermal_state       smallint,                      -- 0 nominal..3 critical; NULL unavailable
  cpu_temp_c          real,                          -- only real sensors
  maestro_cpu_pct     real,  maestro_rss_bytes bigint,
  PRIMARY KEY (agent_run_id, ts)                     -- includes partition key ⇒ allowed [V]
) PARTITION BY RANGE (ts);

CREATE TABLE device_samples (
  ts                  timestamptz NOT NULL,
  device_session_id   uuid NOT NULL,
  agent_run_id        uuid NOT NULL,
  project_id          uuid NOT NULL,
  coalesced_n         smallint NOT NULL DEFAULT 1,
  emu_cpu_pct         real, emu_rss_bytes bigint,    -- HOST-side emulator process
  adb_state           smallint, boot_completed boolean,
  guest_cpu_pct       real, guest_mem_available_bytes bigint,   -- GUEST-side, labelled as such
  PRIMARY KEY (device_session_id, ts)
) PARTITION BY RANGE (ts);
```

### 10.2 Ingestion, partitions and rollups [P]

* **Insertion:** buffer telemetry in memory for 10–30 s, then write each table with one `INSERT … SELECT * FROM unnest($1::timestamptz[], $2::uuid[], …) ON CONFLICT DO NOTHING`. At about 50 rows/s fleet-wide this is plenty, and `COPY` is unnecessary. `COPY FROM STDIN (FORMAT binary)` remains an option for bulk spool imports, but `COPY` does not support `ON CONFLICT` [K], so stage through a temporary table if you use it. Send `telemetry_ack` after the commit.
* **Live view:** the browser receives samples from the in-memory hub immediately. They are not read back from Postgres. Spec §5 requires that browser frequency be independent of retention.
* **Partition manager**, an app background task under a Postgres advisory lock so only one instance runs it. It does three things:
  * `CREATE TABLE IF NOT EXISTS host_samples_p20261104 PARTITION OF host_samples FOR VALUES FROM ('2026-11-04') TO ('2026-11-05')`, plus per-partition `BRIN (ts)`, for today−1 … today+7.
  * Do **not** create a DEFAULT partition. It makes later partition creation scan it under an exclusive lock [V]. Rows with absurd timestamps are rejected at validation instead: the clock offset is checked so that `|offset| ≤ 24 h`, and beyond that the sample is re-stamped with corrected time and flagged.
  * Retention: `ALTER TABLE … DETACH PARTITION … CONCURRENTLY` [V], then `DROP TABLE`. `CONCURRENTLY` cannot run inside a transaction block [K].
* **Partition counts:** about 8 tables × daily or monthly granularity × retention gives on the order of 200 partitions. That is well within the "few thousand" the planner handles [V].
* **pg_partman:** a valid alternative where the managed Postgres or image ships it (5.5.0 [V]). Keep the app's partition manager behind an interface so either can be used.
* **Rollup job:** runs every minute under an advisory lock.
  1. Process window `[watermark, now() − 2 min)`, using `date_bin('1 minute', ts, '2000-01-01')` [K].
  2. Upsert aggregates (`ON CONFLICT (agent_run_id, bucket) DO UPDATE`), then advance `rollup_watermarks.host_1m`.
  3. **Late data:** when ingestion writes samples with `ts < watermark` (replay after reconnect, spool upload), it inserts `(agent_run_id, bucket)` into `rollup_dirty`. The next job run recomputes those buckets from raw data. Raw data is kept for 7 days, which bounds how late data can be repaired. Rollups are idempotent, so recomputation is always safe.
  4. 1 h and daily rollups are computed from 1 m rollups in the same way.
* **BRIN:** used on `ts` inside raw partitions, where insertion order is about the same as time order. It suits the rollup job's range scans at almost no storage cost [K]. Per-run chart queries use the primary-key B-tree `(agent_run_id, ts)`.
* **Compression:** small rows are never TOASTed, so the lz4 default in PG 19 [V] does not help here. Storage is driven by tuple count, which is why the row format is wide.

### 10.3 Volume estimates (arithmetic) [P]

The assumptions are deliberately pessimistic:

* 50 concurrent workers × 4 devices.
* 5 s sampling.
* "24/7" means fully busy all day. "8 h-equiv" means one third of that.
* Wide row ≈ 128 B on the heap, including the 24 B tuple header, 4 B line pointer, ~60 B of fixed columns and NULL bitmap, plus about 48 B of primary-key B-tree. Total ≈ 176 B/row.

| Quantity | Calculation | 24/7 | 8 h-equiv |
|---|---|---|---|
| Samples per source per day | 86,400 / 5 | 17,280 | 5,760 |
| Sources | 50 host + 200 device | 250 | 250 |
| **Raw wide rows/day** | 250 × 17,280 | **4.32 M** | 1.44 M |
| Raw storage/day | 4.32 M × 176 B | **0.76 GB** | 0.25 GB |
| Raw, 7-day retention | × 7 | **5.3 GB** | 1.8 GB |
| (If raw were kept 90 days) | × 90 | 68 GB | 23 GB |
| *Narrow alternative:* series | 50×12 host metrics + 200×8 device metrics | 2,200 | 2,200 |
| *Narrow* rows/day | 2,200 × 17,280 | 38.0 M | 12.7 M |
| *Narrow* 90 days at ~100 B/row | | 342 GB | 114 GB |
| **1-minute rows/day** | 250 × 1,440 | 360 k | 120 k |
| 1-minute, 90 days (~268 B/row incl. index) | 32.4 M rows | **8.7 GB** | 2.9 GB |
| 1-hour rows/year | 250 × 24 × 365 | 2.19 M (0.59 GB) | 0.73 M (0.2 GB) |

**Execution data.** Assume one flow per device per minute with 30 steps.

| Quantity | Calculation | 24/7 | 8 h-equiv |
|---|---|---|---|
| Flow executions/day | 200 × 1,440 | 288 k | 96 k |
| Steps/day | × 30 | 8.64 M | 2.88 M |
| Journal events/day | flows × (2 + 60) | 17.9 M (**207 ev/s**) | 6.0 M (69 ev/s) |
| `agent_events` at ~350 B/row (JSONB + PK) | | 6.2 GB/day → **87 GB @ 14 days** | 2.1 GB/day → 29 GB |
| `step_executions` at ~160 B/row | | 1.4 GB/day → 124 GB @ 90 days | 0.46 GB/day → 41 GB @ 90 days |
| `flow_executions` at ~600 B/row incl. indexes | | 105 M rows/yr ≈ 63 GB/yr | 35 M/yr ≈ 21 GB/yr |

**What this means [P]:**

1. Telemetry is not the storage problem once raw samples are kept only 7 days. **Step-level data and the raw journal are.** Recommendations:
   * Step rows for *passed* flows get 14-day retention, failed flows 90 days. This is enforced by a partition-level split, or simply by dropping step partitions older than 14 days after copying failed-flow steps to `step_executions_failed`.
   * Keep the journal for 14 days (configurable down to 3).
   * Optional later optimisation: store the journal one row per *frame*, with `data` holding the batch compressed. This cuts tuple overhead by about 10–50×.
2. Write rate (~200 rows/s journal plus ~200 rows/s derived plus 50 telemetry rows/s at the upper bound) is far below what a single modest Postgres instance handles [K]. One instance is enough, as spec §6 assumes.
3. A 500 GB volume covers the 24/7 upper bound with the retention above. The realistic case fits in about 150 GB.

---

## 11. Analytics with Postgres alone

### 11.1 Execution-condition fingerprint [P]

`condition_sets.dims` is canonical JSON (RFC 8785 JCS; the `serde-json-canonicalizer` crate is 0.4.1 [V]). `fingerprint = sha256(JCS(dims))`. Dimensions:

| Dimension | Example | Why it is included |
|---|---|---|
| `device_profile` | platform, kind, model/AVD hardware profile, OS version, API level, ABI | Spec §8F: "API 35 vs API 36" and "one emulator type vs another" |
| `app` | app id + build variant + user-supplied `--condition key=value` labels | "App config" is opaque to Fleet, so the user supplies it |
| `maestro` | major.minor | CLI behaviour changes |
| `runner_class` | CI runner label or resource class (for example `macos-14-xlarge`), and host core count bucket | Host capacity affects timing |
| `shard_mode`/`concurrency` | devices per host | Contention affects timing, not correctness. Include it for **duration** comparisons and exclude it for **flakiness** (configurable). |

The flow path is **not** part of the fingerprint; it lives in `flow_definition_id`. Queries group by `(flow_definition_id, condition_set_id)`. Because the dimensions are stored, a coarser grouping can drop dimensions at query time, for example `dims - 'maestro'`.

### 11.2 Queries [P]

**Pass rate.** Indeterminate outcomes are excluded from the denominator and reported separately.

```sql
SELECT fd.repo_path,
       count(*) FILTER (WHERE fe.state = 'passed')                       AS passed,
       count(*) FILTER (WHERE fe.state = 'failed')                       AS failed,
       count(*) FILTER (WHERE fe.state IN ('interrupted','unknown'))     AS indeterminate,
       round(count(*) FILTER (WHERE fe.state = 'passed')::numeric
             / nullif(count(*) FILTER (WHERE fe.state IN ('passed','failed')), 0), 4) AS pass_rate
FROM flow_executions fe
JOIN flow_definitions fd ON fd.id = fe.flow_definition_id
WHERE fe.project_id = $1
  AND fe.ended_at >= now() - interval '30 days'
  AND fe.finality <> 'provisional'
GROUP BY fd.repo_path
ORDER BY pass_rate NULLS LAST;
```

**Flaky rate over comparable conditions.** A "flaky group" is the same flow, same condition set and same commit, with both pass and fail. Failures classified as infrastructure (`device_disconnected`, `emulator_boot_failure`, `infrastructure_failure`) are excluded, because they are a separate signal.

```sql
WITH runs AS (
  SELECT fe.flow_definition_id, fe.condition_set_id, fe.commit_sha, fe.state
  FROM flow_executions fe
  WHERE fe.project_id = $1 AND fe.ended_at >= now() - interval '30 days'
    AND fe.finality IN ('verified','unverified_final')
    AND fe.state IN ('passed','failed')
    AND NOT EXISTS (SELECT 1 FROM failure_classifications c
                    WHERE c.flow_execution_id = fe.id AND c.confidence >= 0.8
                      AND c.category IN ('device_disconnected','emulator_boot_failure','infrastructure_failure'))
), groups AS (
  SELECT flow_definition_id, condition_set_id, commit_sha,
         bool_or(state = 'passed') AS any_pass, bool_or(state = 'failed') AS any_fail
  FROM runs GROUP BY 1, 2, 3 HAVING count(*) >= 2
)
SELECT flow_definition_id, condition_set_id,
       count(*)                                      AS comparable_groups,
       count(*) FILTER (WHERE any_pass AND any_fail) AS flaky_groups,
       count(*) FILTER (WHERE any_pass AND any_fail)::float / count(*) AS flaky_rate
FROM groups GROUP BY 1, 2
HAVING count(*) >= 3
ORDER BY flaky_rate DESC;
```

**Pass-after-retry rate**, using CI re-runs: join `flow_attempt_groups` on `attempt_no = 1` failed and `attempt_no = 2` passed.

**Duration percentiles**, over passed executions only:

```sql
SELECT flow_definition_id, condition_set_id, count(*) AS n,
       percentile_cont(ARRAY[0.5, 0.9, 0.95]) WITHIN GROUP (ORDER BY duration_ms) AS p50_p90_p95
FROM flow_executions
WHERE project_id = $1 AND state = 'passed' AND finality <> 'provisional'
  AND ended_at >= now() - interval '30 days' AND duration_ms IS NOT NULL
GROUP BY 1, 2;
```

**New failure on a branch vs a main baseline**, with explicit "no comparable baseline":

```sql
SELECT cur.id, cur.flow_definition_id, cur.condition_set_id, b.n, b.passed, b.last_state,
       CASE WHEN b.n IS NULL OR b.n < 3                          THEN 'no_comparable_baseline'
            WHEN b.passed = b.n                                  THEN 'new_failure'
            WHEN b.passed = 0                                    THEN 'already_failing_on_main'
            WHEN b.last_state = 'failed'                         THEN 'recently_failing_on_main'
            ELSE 'flaky_on_main' END AS verdict
FROM flow_executions cur
LEFT JOIN LATERAL (
  SELECT count(*) AS n,
         count(*) FILTER (WHERE m.state = 'passed') AS passed,
         (array_agg(m.state ORDER BY m.ended_at DESC))[1] AS last_state
  FROM (SELECT state, ended_at FROM flow_executions m
        WHERE m.project_id = cur.project_id
          AND m.flow_definition_id = cur.flow_definition_id
          AND m.condition_set_id = cur.condition_set_id          -- comparable conditions only
          AND m.is_default_branch
          AND m.state IN ('passed','failed') AND m.finality <> 'provisional'
          AND m.ended_at <  cur.started_at
          AND m.ended_at >= cur.started_at - interval '14 days'
        ORDER BY m.ended_at DESC LIMIT 20) m
) b ON true
WHERE cur.pipeline_attempt_id = $1 AND cur.state = 'failed';
```

When `no_comparable_baseline` comes back, the comparison UI should explain why (spec §8F): look for the same flow under *other* condition sets and list the differing dimensions with a JSONB diff of `dims`. A merge-base baseline (main at the branch point) needs the merge-base SHA, which the agent can compute with `git merge-base` if the checkout has history. Add it as an optional `git.merge_base_sha` in `CiContext`.

**Do we need materialized views or rollups?**

* For windows up to 90 days, the queries above run on the partial covering index `(project_id, flow_definition_id, condition_set_id, ended_at DESC) INCLUDE (…)`. At ≤ 26 M rows (upper bound) they are acceptable for interactive use. Validate in Phase-5 load tests.
* For 1-year trends, use `flow_daily_stats`, an incremental rollup table upserted by the background job when flows finalize, with a nightly full recompute of the last 3 days.
  * Long-window percentiles use a fixed log-scale histogram (`int4[40]`, 100 ms to 1 h). The application interpolates from it to get approximate percentiles without extensions.
* **Avoid** `MATERIALIZED VIEW` for anything large: `REFRESH` recomputes everything. Use one only for small, slow-changing summaries, refreshed `CONCURRENTLY` (which needs a unique index [K]).

---

## 12. Artifacts

### 12.1 Upload flow [P]

1. Agent sends `upload_request{ref, artifact_id, kind, content_type, size, sha256, flow_execution_id?, original_name}`.
2. Server checks the policy allowlist (kind, content type, size cap per kind, per-run byte budget) and authorization (the run owns the referenced flow). It then locks or creates the blob row:
   * `blobs(project_id, sha256)` with `state = 'present'` → `upload_grant{disposition: exists}`. The artifact row is created straight away as `available`. This is the **dedup hit**.
   * Missing → insert as `pending` → `upload_grant{disposition: upload, url: presigned PUT, headers: {x-amz-checksum-sha256, content-length, If-None-Match: *}}`. A fallback mode proxies the upload through the Fleet HTTPS endpoint, `PUT /api/v1/agent/uploads/{artifact_id}`, for IT setups that do not expose object storage to CI.
   * `deleting` → `upload_grant{disposition: retry, retry_after_ms}`.
3. Agent PUTs over HTTPS, never over the WebSocket (spec §6), then sends `upload_complete{ref, artifact_id}`.
4. Server HEADs the object and verifies size, plus the checksum where the store supports it [V AWS / K others]. It then marks the blob `present` and the artifact `available` and emits a live update.
   * A 412 from `If-None-Match` means another uploader won, which counts as success after verification.
   * Stores that do not support conditional writes are handled by the server-side verification step.

### 12.2 Object key layout [P]

```text
v1/{project_id}/cas/sha256/{hex[0:2]}/{hex[2:4]}/{hex64}          # all uploaded content
v1/{project_id}/exports/{export_id}.zip                           # short-lived generated bundles
```

* **No CI or user strings in physical keys.** Flow names and paths are attacker-influenced and can be long, so they stay out. The spec's "organize by project/pipeline/job/execution" is met by `artifacts.logical_path`, which is used for browsing and for **export ZIPs**, whose entries are named `pipeline-142/attempt-1/android-regression/suite-kyc/shard-2/kyc-address-verification/screenshot-3.png`.
* Prefix fan-out (`ab/cd/`) is harmless on S3 and helps filesystem-backed S3-compatible stores [K].
* **Dedup scope is per project.** Global dedup would let a publisher in project A learn that a hash exists in project B from the `exists` disposition, which is an information leak.
* **Downloads:** the server checks authorization, writes an `audit_events` row where required (spec §10), and returns a short-lived presigned GET with `response-content-disposition=attachment`. HTML reports are served from a separate sandbox origin or as attachments only (spec §10). Browsers never receive storage credentials.

### 12.3 Retention classes [P]

| Class | Default TTL | Assigned when |
|---|---|---|
| `report` (JUnit, final HTML report) | 1 year | Upload |
| `failure_evidence` (screenshots and recordings of failed flows, failure log excerpts) | 90 days | **Promoted when the flow finalizes as `failed`** |
| `success_evidence` | 7 days | Default for screenshots and recordings at upload, because the outcome is not yet known |
| `debug_log` (`maestro.log`) | 30 days (90 if any flow in the suite failed) | Upload, then promoted |
| `video` | 14 days, with a size cap per policy | Upload |
| `bundle` | 14 days | Upload |

Promotion runs in the same transaction that sets a flow's `finality ∈ {verified, unverified_final}` with `state = 'failed'`:

```sql
UPDATE artifacts a SET retention_class = 'failure_evidence',
       expires_at = a.created_at + rc.ttl
  FROM retention_classes rc
 WHERE a.flow_execution_id = $fe AND a.retention_class = 'success_evidence'
   AND rc.project_id = a.project_id AND rc.class = 'failure_evidence';
```

A later demotion, for example if a re-reconciliation flips the flow to passed, is allowed but never shortens `expires_at` below `now() + 24h`.

### 12.4 Garbage collection [P]

```text
every 10 min (advisory lock):
  1. expire:  UPDATE artifacts SET state='expired' WHERE state='available' AND NOT legal_hold
              AND expires_at < now() RETURNING project_id, sha256         (batch 1000)
  2. mark:    for each (project_id, sha256) returned:
                UPDATE blobs b SET unreferenced_since = now()
                 WHERE (b.project_id, b.sha256) = (...) AND b.unreferenced_since IS NULL
                   AND NOT EXISTS (SELECT 1 FROM artifacts a WHERE a.project_id=b.project_id
                                   AND a.sha256=b.sha256 AND a.state IN ('declared','uploading','available'))
  3. sweep:   SELECT … FROM blobs WHERE unreferenced_since < now() - interval '24 hours'
              AND state='present' FOR UPDATE SKIP LOCKED LIMIT 100
              → re-check NOT EXISTS (race with a new upload_grant that cleared unreferenced_since)
              → state='deleting' (commit) → DELETE object → DELETE blob row
              (crash between steps is safe: 'deleting' rows are retried; object 404 = done)
  4. orphans: daily, list objects under v1/{project}/cas/ older than 48 h with no blob row → delete
              (aborted uploads); plus bucket lifecycle rule AbortIncompleteMultipartUpload [V]
  5. expired artifact rows are deleted after 30 days (tombstone kept for audit/history UI)
```

* The upload path clears `unreferenced_since` under the same row lock. A blob in `deleting` returns `retry`. Together these close the race between delete and re-reference.
* Counting references with `NOT EXISTS` over the indexed `artifacts(project_id, sha256)` avoids hot refcount columns.

---

## 13. Multi-tenancy and authorization model [P]

* **Projects** are the tenancy unit. Every operational row carries `project_id`.
* **Users** come from OIDC `(issuer, subject)`. **Role bindings** apply to a user or to an **OIDC group claim** (so IT manages membership in the IdP), per project or globally (`project_id IS NULL`).
  * `viewer`: read history, live view and artifacts.
  * `maintainer`: viewer plus project settings, flow tags, retention within limits, and publisher credentials for the project.
  * `admin`: everything, including global settings, audit view and cross-project analytics.
  * The effective role is the maximum over bindings.
* **Publisher credentials** are always bound to exactly one project, with publish-only scope. They grant **only** `/api/v1/agent/*`: the stream, the token exchange and uploads for the caller's own run. They never read history or administration (spec §10).
  * **`oidc_trust` (preferred):**
    1. The agent fetches a GitHub OIDC JWT with `audience = fleet`.
    2. It calls `POST /api/v1/agent/token`.
    3. The server verifies the issuer and JWKS, checks `claim_rules` (for example `repository_id`, a `ref` glob, `event_name` not `pull_request_target`, `runner_environment`), and issues a short-lived Fleet publish token bound to `(project_id, run_id, run_attempt, check_run_id)`.
    4. **CI natural keys are taken from the verified claims** [V: claims exist] and override anything the agent claims.
  * **`static_token` (fallback):** a 256-bit random secret shown once and stored as `sha256` with a prefix such as `mfp_…`. It can be rotated (audit event) and given an expiry.
  * **Untrusted PRs:** `allow_untrusted_prs = false` by default. The OIDC `event_name` or `ref` rules reject fork-PR tokens, or route them to a quarantined project with a short artifact TTL, depending on IT policy (spec §10).
* **Authorization on ingest:** each frame belongs to a connection authenticated for `(project, job)`. Every entity upsert checks ownership (§9.3b). Upload grants only cover artifacts of the caller's agent run.
* **Row-level security** is optional defense in depth. Enforce in the application first. With pooled connections, RLS needs `SET LOCAL app.project_ids = …` per transaction. That works but adds complexity, so leave it for Phase 5.
* **Audit** covers credential create, rotate and revoke, role binding changes, retention or artifact policy changes, artifact downloads (configurable), and rejected `foreign_entity` attempts.

---
## 14. Spec gaps and ambiguities found

| # | Spec location | Gap or ambiguity | Proposed resolution (section) |
|---|---|---|---|
| G1 | §5 "Each job establishes one connection" | A job can run `maestro-fleet run` more than once (sequentially or in parallel), and the agent can restart. One connection per job is neither achievable nor desirable. | N agent runs per job, each with its own seq space and connection (§3.1, §3.11) |
| G2 | §5 "sequence within its agent session" | "Agent session" is undefined: process? connection? job? Seq continuity across process restarts is impossible without persistence (the same limit as Kafka's per-session idempotence) [V]. | `agent_run` = process. `previous_agent_run_id` + spool replay (§3.11) |
| G3 | §5 Reconnect step 3: agent "reports the last acknowledged sequence" | A lost ack makes the agent's view stale. The server must be authoritative. | `welcome.committed_through` (§3.4) |
| G4 | §5 "bounded local queue" plus "preserve lifecycle events whenever possible" | Behaviour on overflow is undefined. Memory vs disk is undefined, which matters for agent crashes. | Disk spool plus a sequenced `journal.gap` marker (§3.2) |
| G5 | §5 "discard or coalesce low-priority metric samples first" | Metrics share the seq space with lifecycle events, so discarding samples creates gaps | Separate telemetry stream with `tseq` (§3.2) |
| G6 | §5 step 5 "Receive the current authoritative state" | Content and purpose are undefined | Advisory `snapshot` that the agent never uses for test truth (§3.6) |
| G7 | §5 example event | Redundant `event_id`; human-string pipeline and job IDs; `device_id` = ADB serial (reused across jobs and hosts); `flow_id` = path (definition, not execution); undefined `attempt`; provenance buried in the payload; wall clock only | §5.1 |
| G8 | §5 event list | Missing: plan, report parsed or absent, process exit, CI signal, artifact uploaded or rejected, device lost, journal gap, parser drift. `flow.completed`/`flow.failed` split the outcome across types. | §5.2 |
| G9 | §5 `schema_version` per event | A global version on every event conflates envelope and payload evolution | Connection `proto` plus per-type `v` (§4.3) |
| G10 | §3 / §7 `Attempt`, `flow_attempts` | Maestro has no CLI flow retries and the spec forbids automatic retries, so the source of attempts is undefined. It overlaps with GitHub `run_attempt`. | Derived `flow_attempt_groups` view across CI attempts and invocations (§9.2) |
| G11 | §7 "Pipeline ID must distinguish attempts" | On GitHub, re-running failed jobs creates attempt N, and the UI may also show jobs that were *not* re-run under it [K]. If Fleet only sees re-run jobs, attempt N looks "smaller". | Pipeline run plus attempt modelled separately. Pipeline-level views aggregate the latest attempt per `job_key` (§9.2). |
| G12 | §12 CI metadata | `GITHUB_JOB` is not unique across matrix legs, and no default env var exposes the numeric job ID [V] | `job_key` derivation using `job.check_run_id` or the OIDC claim (§3.1) |
| G13 | §6 Job states | Lifecycle and outcome are mixed. "completed" is ambiguous: Maestro passed, or the job finished? It is undefined when a job is "over" if several agent runs are possible. | Lifecycle + outcome, quiet period, `final_for_job`, reopen (§6.3) |
| G14 | §8B "Final CI result" | Cannot be known without provider integration. Spec §12 acknowledges this, but the dashboard header lists it anyway. | Show `ci_result` only when a provider integration fills it. Otherwise show "Maestro result" only. |
| G15 | §4 / §13 "Partial report" | Maestro writes the JUnit report **once, after all shards complete** [V]. A killed Maestro usually leaves **no** report, so "partial" mostly means truncated or corrupt. | Streaming salvage parser plus `report_status` (§8.3) |
| G16 | §4 statuses: "Skipped" | Maestro's JUnit has no `<skipped/>`, and `FlowStatus` has no SKIPPED [V]. Skips are only knowable from plan vs execution or the sequential-abort message. | §6.5 (`skipped` only with evidence, otherwise `unknown`) |
| G17 | JUnit timestamps | Local time, no offset, truncated to seconds [V]. Unsafe for cross-host timelines. | Agent converts using its own time zone. Prefer live mono timing (§5.6). |
| G18 | §7 "normalized repository-relative path" | Maestro reports paths relative to *its CWD* or as absolute paths [V], so the repo root must be known | Agent normalises using the repo root (§8.1) |
| G19 | §4 "Accurate progress" | The planned total for `--shard-split`/`--shard-all` is not specified as an observable | Maestro prints plan lines ("Will split N flows across M shards…") [V, source]. Treat them as `live_console` plan evidence and validate in Phase 0 (§8.1). |
| G20 | §5 "Durable metrics checkpoint ~30–60 s" | Unclear whether this is agent→server batching, server→DB flush, or a rollup | Telemetry is flushed and acked by the server every 10–30 s (§3.2) |
| G21 | §7 Metrics retention | "5-second readings during live execution" and "90 days of detailed historical metrics" conflict on what "detailed" means | Raw 5 s for 7 days; 1 min for 90 days; 1 h and daily for 1 year (§10) |
| G22 | §7 table list | `ingestion_cursors` duplicates `agent_sessions`. Missing: attempts, device profiles, condition sets, evidence or claims, failure classifications, credentials, role bindings, blobs, rollups, ingest gaps and rejections. | §9.2 mapping table |
| G23 | §9 "Flakiness over comparable conditions" | "Comparable" is undefined | Condition fingerprint (§11.1) |
| G24 | §13 "Duplicate or out-of-order messages: reject stale transitions" | "Stale" is undefined. Late evidence after a timeout should *upgrade* inferred states, not be rejected. | Claim lattice with ranks (§7) |
| G25 | §10 Authorization | Agent-chosen entity IDs could collide with or target another project's rows | Ownership check on conflict plus audit (§9.3) |
| G26 | §7 Artifact retention | "Failure screenshots retain longer" cannot be decided at upload time, because the outcome is not known yet | Retention promotion at finalization (§12.3) |
| G27 | §7 "optionally deduplicate identical content" | Cross-project dedup leaks existence of content | Per-project CAS (§12.2) |
| G28 | §3 Device | "emulator-5556" is an attribute that can be reused, not an identity. A device "across jobs" only makes sense as a *profile*. | `device_sessions` + `device_profiles` (§9.2) |
| G29 | §6 Live state "version numbers or event sequences" | Not specified per entity | `rev` per entity, bumped only on real change (OLD/NEW `RETURNING`) (§9.3) |
| G30 | §16 #15 "cannot appear falsely successful" | Needs a mechanical guarantee | DB `CHECK` on `flow_claims` forbids `passed` from inferred sources; enum fallback maps to `unknown` (§4.3, §9.2) |

---

## 15. Items to validate in Phase 0 (fixtures against Maestro 2.12.x)

1. The exact console lines for flow start and end, per-shard prefixes (`[shard N]`), plan lines, and the sequential-abort message. Their stability across 2.10–2.12.
2. The `--test-output-dir` contents (`manifest.json`, `commands.json`, `logs/`, `takeScreenshot/`, per the CLI help text [V]). Whether per-command status and duration are reliable enough to be `artifact_file` rank-40 evidence for steps and flows.
3. When `WARNING` is emitted, and whether `CANCELED`/`STOPPED` ever appear in local (non-cloud) JUnit output.
4. JUnit behaviour on SIGTERM and SIGKILL to Maestro (expected: no file), and on disk-full (truncated file).
5. Whether `device.description` in `<testsuite device=…>` is stable enough to map testsuites to shards, compared with the serial the agent knows.
6. Ordering of `<testcase>` under `executionOrder`/sequential config with `continueOnFailure: false`.
7. Measure real event rates (steps per flow, flows per minute) to replace the §10.3 assumptions.

---

## 16. Sources

PostgreSQL
- Versioning policy (supported versions and EOL dates): https://www.postgresql.org/support/versioning/
- PostgreSQL 18 release announcement: https://www.postgresql.org/about/news/postgresql-18-released-3142/
- PostgreSQL 18 release notes: https://www.postgresql.org/docs/18/release-18.html
- UUID functions (`uuidv7`, `uuid_extract_timestamp`): https://www.postgresql.org/docs/18/functions-uuid.html
- UUID type: https://www.postgresql.org/docs/18/datatype-uuid.html
- `uuidv7()` commit (monotonic within a backend, 12-bit sub-ms): https://www.postgresql.org/message-id/E1tLWXf-002GcM-A3%40gemulon.postgresql.org
- Commentary on UUIDv7 monotonicity: https://brandur.org/fragments/uuid-v7-monotonicity
- Table partitioning (unique-key limitation, DETACH CONCURRENTLY, partition counts): https://www.postgresql.org/docs/18/ddl-partitioning.html
- PostgreSQL 19 RC 1 announcement (GA planned 2026-11-19): https://www.postgresql.org/about/news/postgresql-19-rc-1-released-3409/
- PostgreSQL 19 release notes (ON CONFLICT DO SELECT, COPY JSON, lz4 TOAST default, REPACK): https://www.postgresql.org/docs/19/release-19.html
- PostgreSQL 19 Beta 1 announcement: https://postgresql.org/about/news/postgresql-19-beta-1-released-3313/
- pg_partman releases: https://github.com/pgpartman/pg_partman/releases

UUIDs and canonical JSON
- RFC 9562 (UUIDv7, monotonicity methods, DB-key guidance): https://www.rfc-editor.org/rfc/rfc9562.html
- Rust `uuid` crate `now_v7` docs: https://docs.rs/uuid/latest/uuid/struct.Uuid.html#method.now_v7
- crates.io API, used for current versions of schemars, uuid, serde_json, rmp-serde, ciborium, prost, postcard, bincode, utoipa, sqlx, serde-json-canonicalizer, ulid: https://crates.io/api/v1/crates/{name}

Reliable-delivery designs
- MQTT 5.0 (OASIS standard): https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.html
- Kafka KIP-98 (idempotent producer, PID, epoch, sequence): https://cwiki.apache.org/confluence/display/KAFKA/KIP-98+-+Exactly+Once+Delivery+and+Transactional+Messaging
- NATS JetStream headers (`Nats-Msg-Id`, expected-sequence headers): https://docs.nats.io/reference/jetstream/api/headers
- NATS dedup window (secondary sources for the 2-minute default): https://www.synadia.com/insights/checks/nats-large-deduplication-window , https://docs.feldera.com/connectors/sinks/nats/
- Socket.IO connection-state recovery: https://socket.io/docs/v4/connection-state-recovery
- Phoenix Channels guide (at-most-once delivery, rejoin): https://phoenix.hexdocs.pm/channels.html
- Phoenix channels client protocol (message format, heartbeat): https://phoenix.hexdocs.pm/writing_a_channels_client.html
- OTLP specification (retry, throttling, partial success): https://opentelemetry.io/docs/specs/otlp/
- nginx WebSocket proxying (60 s idle close, ping frames): https://nginx.org/en/docs/http/websocket.html

Serialization
- schemars documentation: https://graham.cool/schemars/
- bincode 3.0.0 README (development ceased): https://docs.rs/crate/bincode/latest/source/README.md
- RUSTSEC-2025-0141 context (bincode unmaintained): https://github.com/argumentcomputer/ix/issues/291

Maestro (source read at `main`, 2026-11-03)
- JUnit reporter: https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/report/JUnitTestSuiteReporter.kt
- Timestamp formatting (local, no offset, seconds): https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/report/TestSuiteReporter.kt
- FlowStatus enum: https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/model/FlowStatus.kt
- TestExecutionSummary model: https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/model/TestExecutionSummary.kt
- TestCommand (sharding, plan messages, merge and save report once, exit code): https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/command/TestCommand.kt
- TestSuiteInteractor (per-shard suite, shard prefixes, sequential abort): https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/runner/TestSuiteInteractor.kt
- TestDebugReporter (per-flow dirs with `-shard-N` suffix): https://github.com/mobile-dev-inc/maestro/blob/main/maestro-cli/src/main/java/maestro/cli/report/TestDebugReporter.kt
- Releases (CLI 2.12.1, 02 Nov): https://github.com/mobile-dev-inc/maestro/releases

GitHub Actions
- Default variables (`GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`, `GITHUB_JOB`, …): https://docs.github.com/en/actions/reference/workflows-and-actions/variables
- OIDC token claims (`check_run_id`, `run_id`, `run_attempt`, `repository_id`, …): https://docs.github.com/en/actions/reference/security/oidc
- `job.check_run_id` context (docs issue referencing its addition): https://github.com/github/docs/issues/40669

Object storage
- S3 conditional writes (`If-None-Match: *`, 412/409 semantics): https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html
- S3 object integrity (SHA-256 checksums): https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity.html
