# Runner guide

## Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/moyaver/fleet/releases/latest/download/fleet-agent-installer.sh | sh
```

Release archives are built for Linux x86_64 and aarch64 (static, musl) and macOS on Apple Silicon. Each comes with a SHA-256 checksum, a GitHub artifact attestation, the license and the third-party notices.

## Check the setup

```bash
fleet-agent check
```

It checks that `maestro` is on `PATH` (and which version), that the adb server is listening, that the Fleet server is reachable, and that the run directory is writable. Unlike `run`, it exits 1 on any problem.

## adb

The agent reads the device list from the adb server (`host:track-devices`) and never starts, restarts or kills the adb server: a different `adb` client version could restart it and break Maestro. If nothing listens on port 5037 (or `ANDROID_ADB_SERVER_PORT`), the agent waits.

## Cost

Measured on a real 4-flow Maestro 2.12.1 suite (50 s, Apple M1 Pro): the agent used 0.07 s of CPU in total, about 0.14% of one core, and the suite took the same time with and without it. Its own CPU and memory are reported with every metrics sample.

## Signals and CI cancellation

On the first INT or TERM the agent forwards the signal to Maestro's process group and starts a grace timer (`FLEET_GRACE`); when it expires, the group is killed. Maestro itself shuts down in under a second (research 08). If the agent's supervisor dies, the observer terminates Maestro so no orphan JVM is left behind.

## Credentials

On GitHub Actions, give the job `permissions: id-token: write` and leave `FLEET_TOKEN` unset: the agent asks the runner for an OIDC token (audience `FLEET_OIDC_AUDIENCE`, default `fleet`) and exchanges it at `POST /api/v1/agent/token` for a short-lived agent token. The server decides the project and the trust level from the token's verified claims (repository and owner IDs, event, ref) and its trust policies; the job key comes from the verified `check_run_id`. Fork pull requests get no OIDC token and run offline.

Elsewhere, set `FLEET_TOKEN` to an API key (`fleet api-key create --project <slug> --name <ci>` on the server). Either way, a refused credential only turns telemetry off: Maestro runs and exits exactly as it would without the agent.

## Proxies and certificates

The agent trusts Mozilla's bundle, the system's certificate store and, with `FLEET_CA_FILE`, an extra PEM bundle (for TLS-inspecting corporate proxies). `wss://` connections go through `HTTPS_PROXY` (or `ALL_PROXY`), `ws://` through `HTTP_PROXY`, unless `NO_PROXY` matches the host (`host`, `.domain`, `domain` or `*`). The WebSocket opens an HTTP `CONNECT` tunnel; proxies are `http://`, optionally with `user:password@` (percent-encoded). The OIDC exchange uses the same proxies and certificates.
