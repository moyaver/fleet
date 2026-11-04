# Protocol reference

The agent ↔ server wire protocol ([ADR-0005](https://github.com/moyaver/fleet/blob/main/docs/decisions/0005-wire-protocol.md), research 05 §3). The Rust types in `fleet-protocol` are the definition.

## Transports

- **WebSocket** `GET /api/v1/agent/ws`, subprotocol `fleet.v1+json`, `Authorization: Bearer <API key or agent token>`. One JSON object per text frame, tagged by `op`.
- **HTTPS batch** `POST /api/v1/agent/batch` for networks that block WebSockets: `{hello, events, telemetry}` in, `{frames}` out (the frames a socket would have carried).
- **Credentials** `POST /api/v1/agent/token`: `{id_token}` (a CI OIDC token) or an API key as the bearer, out comes a short-lived agent token.

## Session

1. `hello` (agent run ID, resume state, CI context) → `welcome` (`epoch`, `committed_through`, limits) or `reject` (`fatal` means stop for good; telemetry turns off, Maestro is unaffected).
2. The agent drops everything up to `committed_through` and sends the rest as `events` batches, contiguous in `seq`.
3. Each batch is one database transaction; after the commit the server sends `ack{through}` (cumulative). Duplicates are acked again and counted. A permanently refused event gets `event_rejected` and the cursor moves past it. A batch that skips ahead gets `error{seq_gap, expected_seq}` and the socket stays open.
4. `telemetry` is a separate stream, written at a checkpoint and acked with `telemetry_ack`.
5. A newer connection for the same run bumps the epoch; the older one gets `error{superseded}` and close 1008.
6. On shutdown the server sends `goaway` and closes with 1012; the agent reconnects after the given delay.

Gaps are never silent: lost prefixes and `journal.gap` events are stored as gaps and shown as missing evidence.
