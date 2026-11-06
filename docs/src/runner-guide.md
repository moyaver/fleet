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

`cargo xtask bench-overhead` measures the agent's cost on the captured four-flow suite the agent's tests replay, with a fake Maestro. It runs the suite with and without `fleet-agent run` (offline, no server), alternating the order, and reports wall time, the agent's CPU time and its peak memory. `--runs N` (default 5) sets the number of runs, `--linger SECS` (default 10) how long each run lasts, and `--max-cpu-percent X` makes it exit 1 when the agent uses more than X% of one core. The numbers depend on the machine, so the command is not part of `cargo xtask ci`.

Measured with `cargo xtask bench-overhead --runs 3 --linger 50` on an Apple M1 Pro (10 cores, macOS 26.7.1), release build, 50 s per run, about the length of the real four-flow suite:

- agent CPU: 99 ms per run (range 86 to 119 ms), 0.20% of one core and 0.02% of the host;
- agent peak memory: 17.2 MiB, supervisor and observer together;
- duration: the run with the agent took 0.32 s longer (range 0.27 to 0.34 s), 0.6% of 50 s. Most of that is probably the observer's 250 ms wait for Maestro's last writes after Maestro exits (`crates/fleet-agent/src/observer.rs`).

Shorter runs spend more of their time on the fixed start-up and exit cost: the default 10 s runs used 0.42% and 0.43% of one core in two runs on the same machine. Its own CPU and memory are also reported with every metrics sample.

## Signals and CI cancellation

On the first INT or TERM the agent forwards the signal to Maestro's process group and starts a grace timer (`FLEET_GRACE`); when it expires, the group is killed. Maestro itself shuts down in under a second (research 08). If the agent's supervisor dies, the observer terminates Maestro so no orphan JVM is left behind.

## Credentials

On GitHub Actions, give the job `permissions: id-token: write` and leave `FLEET_TOKEN` unset: the agent asks the runner for an OIDC token (audience `FLEET_OIDC_AUDIENCE`, default `fleet`) and exchanges it at `POST /api/v1/agent/token` for a short-lived agent token. The server decides the project and the trust level from the token's verified claims (repository and owner IDs, event, ref) and its trust policies; the job key comes from the verified `check_run_id`. Fork pull requests get no OIDC token and run offline.

Elsewhere, set `FLEET_TOKEN` to an API key (`fleet api-key create --project <slug> --name <ci>` on the server). Either way, a refused credential only turns telemetry off: Maestro runs and exits exactly as it would without the agent.

## Proxies and certificates

The agent trusts Mozilla's bundle, the system's certificate store and, with `FLEET_CA_FILE`, an extra PEM bundle (for TLS-inspecting corporate proxies). `wss://` connections go through `HTTPS_PROXY` (or `ALL_PROXY`), `ws://` through `HTTP_PROXY`, unless `NO_PROXY` matches the host (`host`, `.domain`, `domain` or `*`). The WebSocket opens an HTTP `CONNECT` tunnel; proxies are `http://`, optionally with `user:password@` (percent-encoded). The OIDC exchange uses the same proxies and certificates.
