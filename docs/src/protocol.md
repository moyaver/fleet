# Protocol reference

The agent ↔ server wire protocol ([ADR-0005](https://github.com/moyaver/fleet/blob/main/docs/decisions/0005-wire-protocol.md), research 05 §3). The Rust types in `fleet-protocol` are the definition.

## Transports

- **WebSocket** `GET /api/v1/agent/ws`, subprotocol `fleet.v1+json`, `Authorization: Bearer <API key or agent token>`. One JSON object per text frame, tagged by `op`.
- **HTTPS batch** `POST /api/v1/agent/batch` for networks that block WebSockets: `{hello, events, telemetry}` in, `{frames}` out (the frames a socket would have carried).
- **Credentials** `POST /api/v1/agent/token`: `{id_token}` (a CI OIDC token) or an API key as the bearer, out comes a short-lived agent token.

## Artifact uploads

Artifact bytes travel over HTTPS, never the WebSocket (`fleet_protocol::artifact` defines the types). Every call carries the agent's bearer credential.

1. `POST /api/v1/agent/artifacts` with `{agent_run_id, flow_execution_id?, class, path, content_type, size, sha256}` answers with a tagged `disposition`: `upload` (a grant: `url`, `complete_url`, `expires_at`, `size`; HTTP 201), `exists` (the project already holds these bytes for this class: nothing to send) or `rejected` (`reason`, `message`). The request is **idempotent on `(agent_run_id, class, sha256)`**: asking again returns the same artifact, or a fresh grant if the earlier one lapsed or the transfer failed. HTTP 409 (`unknown_run`, `unknown_flow`) means the run or flow has not reached the server yet: retry shortly.
2. `PUT <url>` with exactly `size` bytes and a `Content-Length`. The grant is bound to the credential that asked, is short-lived (`artifacts.grant_ttl_seconds`) and accepts the bytes **once**: a second `PUT` is 409, a lapsed grant 410, too many bytes 413. A failed transfer leaves the grant usable. Bytes are spooled, not stored.
3. `POST <complete_url>` verifies size, SHA-256, magic bytes for the class and archive safety, then writes the content-addressed blob. It answers `{status: available}` or `{status: rejected, reason}`; nothing of a rejected upload is kept, and asking again is refused the same way.

**Direct upload (S3 only).** An agent that sends `"direct": true` in its request (this build's agents do) may be granted `presigned` instead of `upload`: `url` (absolute, the object store's), `headers` (send exactly these, for example `x-amz-checksum-sha256`, the base64 of the SHA-256), `complete_url`, `expires_at`, `size`. The agent `PUT`s the bytes straight to `url` with a `Content-Length` and **no** bearer credential, then does step 3 as usual. The server reads the staged object once, at `complete`, into its spool and verifies it exactly as a relayed body. A store that cannot presign answers `upload`, and an agent whose direct `PUT` fails (refused, unreachable, or a URL that is not `http(s)`) sends the same file through the relayed path. An agent that does not send `direct` never sees `presigned`, so older agents are unaffected.

Classes (the allowlist): `failure_screenshot` (PNG, JPEG, WebP), `failure_hierarchy` (JSON: the failing step's UI tree, its text values redacted by the agent), `commands` (text), `junit` (XML), `maestro_log` (text), `html_report` (HTML, or a zip), `device_logcat` (text: the excerpt of a failed flow's device log, at most 512 KiB, redacted by the agent). Anything else is `rejected` with `class_not_allowed`. Reasons: `class_not_allowed`, `content_type_not_allowed`, `too_large`, `run_quota_exceeded`, `size_mismatch`, `digest_mismatch`, `type_not_allowed`, `not_text`, `unsafe_archive`.

The agent journals the outcome in its own event stream: `artifact.uploaded` (`class`, `path`, `artifact_id`, `size`) or `artifact.rejected` (`class`, `path`, `reason`: a server reason, `upload_failed`, or `not_redactable` for a text the agent will not send because it cannot redact it reliably). The server's artifact table is what the dashboard reads.

## Session

1. `hello` (agent run ID, resume state, CI context) → `welcome` (`epoch`, `committed_through`, limits) or `reject` (`fatal` means stop for good; telemetry turns off, Maestro is unaffected).
2. The agent drops everything up to `committed_through` and sends the rest as `events` batches, contiguous in `seq`. A `step.started` may carry optional `depth`, `parent` and `attempt` for steps nested by `runFlow` and for retries (#134); top-level steps carry none of them.
3. Each batch is one database transaction; after the commit the server sends `ack{through}` (cumulative). Duplicates are acked again and counted. A permanently refused event gets `event_rejected` and the cursor moves past it. A batch that skips ahead gets `error{seq_gap, expected_seq}` and the socket stays open.
4. `telemetry` is a separate stream, written at a checkpoint and acked with `telemetry_ack`. Its optional `devices` list carries each device's adb state, boot completion and guest CPU and available memory; unknown readings are left out (#136). Each host sample carries `unavailable`, the reason for each host or process reading it lacks (`no_system_info`, `no_psi`, `no_process`, `probe_failed`, `no_baseline`; see `SampleUnavailable` in the schemas), as the device samples do (#39). Each device sample also carries its `shard` and, for each reading it lacks, an `unavailable` reason (`not_an_emulator`, `probe_failed`, `throttled`, …; see `DeviceUnavailable` in the schemas). Both are optional, so older agents still send frames that decode (#39).
5. A newer connection for the same run bumps the epoch; the older one gets `error{superseded}` and close 1008.
6. On shutdown the server sends `goaway` and closes with 1012; the agent reconnects after the given delay.

Gaps are never silent: lost prefixes and `journal.gap` events are stored as gaps and shown as missing evidence.

The agent's outbox is bounded (a 64 MiB spool, a small in-memory window). If it runs out of room it stops storing event bodies but keeps their `seq` numbers: the dropped events leave a hole, and the next event it stores (or the end of the run) is preceded by a `journal.gap` sequenced in that hole, at `seq == to_seq`. Its `from_seq..=to_seq` cover the dropped events and the marker's own slot. The server accepts such a marker when `from_seq` is the next `seq` it expects and advances past the hole; any other skip is still a `seq_gap`. So `seq` is gap-free except for holes a `journal.gap` covers, and the numbers in a gap are the same ones the agent's `journal.jsonl` and the stored events carry.
