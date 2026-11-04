# Operator guide

## Deploy

The reference deployment is Docker Compose: Caddy (TLS, WebSocket-friendly), the Fleet server, Postgres 18 and Garage (S3-compatible, used for artifacts from M5). Step-by-step: [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md). The image is built from [`deploy/docker/Dockerfile`](https://github.com/moyaver/fleet/blob/main/deploy/docker/Dockerfile) (cargo-chef, distroless, runs as non-root).

## The `fleet` binary

| Command | Does |
|---|---|
| `fleet serve` | Serves the dashboard, the agent protocol and the API |
| `fleet migrate` | Applies database migrations and exits |
| `fleet check-config` | Validates the configuration and prints it, secrets redacted |
| `fleet healthcheck` | Exits 0 if the local server is ready (`/readyz`); for Docker `HEALTHCHECK` |
| `fleet api-key create --project <slug> --name <n>` | Prints a new API key once (`mf_live_…`); only its hash is stored |

## Configuration

Defaults, then a TOML file (`--config` or `FLEET_CONFIG`), then `FLEET_*` variables (`__` separates sections: `FLEET_DATABASE__URL`), then `FLEET_*_FILE` variables naming a file that holds the value (Docker and Kubernetes secrets). Errors name the variable or key at fault.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Address to listen on (the image sets `0.0.0.0:8080`) |
| `database.url` | — | `postgres://…` (use `FLEET_DATABASE__URL_FILE`) |
| `database.migrate` | `auto` | `auto` migrates on start; `manual` refuses an outdated or newer schema |
| `ingest.heartbeat_interval_ms` | `10000` | Agent heartbeat cadence (sent to agents in `welcome`) |
| `ingest.ping_interval_ms` | `20000` | Server pings, below common 60 s proxy idle cuts |
| `ingest.idle_timeout_ms` | `45000` | A silent agent socket is closed and its run detached |
| `ingest.telemetry_flush_ms` | `15000` | Telemetry checkpoint (10–30 s), then `telemetry_ack` |
| `ingest.shutdown_grace_ms` | `15000` | Drain time for sockets on SIGTERM |
| `liveness.stale_after_ms` | `25000` | A worker shows `stale` (display only) |
| `liveness.resume_grace_ms` | `900000` | A detached run is abandoned; its open flows become `unknown` |
| `api.viewer_tokens` | — | Bearer tokens that may read (until browser SSO in M4) |
| `api.public_url` | — | The dashboard's origin; browser sockets from other origins are refused |
| `oidc.issuers` | GitHub, audience `fleet` | CI OIDC issuers accepted by `POST /api/v1/agent/token` |
| `projects[].trust_policies[]` | — | Who may publish with an OIDC token (repository and owner IDs, trusted refs, PR policy) |

## Shutdown and upgrades

On SIGTERM the server stops accepting connections, finishes each agent's in-flight batch, sends `goaway`, writes the last telemetry checkpoint and closes sockets with 1012; agents reconnect and resume where the server says. All state is in Postgres, so a restart loses nothing that was acknowledged. Give the container a stop grace period of 30 s.

Back up first, then upgrade the server (migrations run, or run `fleet migrate`), then the agents.

## Load testing

`fleet-loadgen` drives simulated agents through the real protocol (`--agents`, `--devices`, `--storm` for a reconnect storm, `--duplicates` for resent and reordered batches) and fails unless every event is acked. Against a deployment behind TLS use `wss://` and point `SSL_CERT_FILE` at the proxy's CA if it is not publicly trusted; see [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md#load-test-a-deployment).
