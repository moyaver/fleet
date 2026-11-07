# Runner guide

## Install

This installs the alpha pre-release `v0.1.0-alpha.3` (pinned, because `latest` skips pre-releases):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/moyaver/fleet/releases/download/v0.1.0-alpha.3/fleet-agent-installer.sh | sh
```

Release archives are built for Linux x86_64 and aarch64 (static, musl) and macOS on Apple Silicon. Each comes with a SHA-256 checksum (also listed in `sha256.sum`), a GitHub artifact attestation, the license and the third-party notices. To verify a downloaded archive with the [GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify fleet-agent-x86_64-unknown-linux-musl.tar.xz --repo moyaver/fleet
```

## GitHub Actions

The [composite action](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent/action.yml) installs the agent (its archive's checksum is verified, and its build attestation too when the `gh` CLI is on the runner), runs a Maestro command under it, and appends a job summary to the run page: the outcome counts, the flows that did not pass with their reason, and a link to the dashboard when `FLEET_SERVER` is set. Give the job `permissions: id-token: write`:

```yaml
- uses: moyaver/fleet/examples/github-actions/fleet-agent@v0.1.0-alpha.3
  with:
    command: maestro test .maestro/
```

The summary is included from `v0.1.0-alpha.2`; `v0.1.0-alpha.1` predates it. The example workflow is [`examples/github-actions/fleet-agent.yml`](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent.yml).

## Check the setup

```bash
fleet-agent check
```

It checks that `maestro` is on `PATH` (and which version), that the adb server is listening, that the Fleet server is reachable, and that the run directory is writable. Unlike `run`, it exits 1 on any problem.

## adb

The agent reads the device list from the adb server (`host:track-devices`) and never starts, restarts or kills the adb server: a different `adb` client version could restart it and break Maestro. If nothing listens on port 5037 (or `ANDROID_ADB_SERVER_PORT`), the agent waits.

Every 30 seconds the agent also probes each device that Maestro selected, with read-only adb shell commands (boot status, and CPU and available memory from `/proc`). The first probe of a device also reads its API level (`ro.build.version.sdk`). A probe has a 5-second timeout. A probe that fails or hangs leaves its readings unknown and never affects Maestro. Devices that Maestro did not select are not probed.

For an emulator, the agent finds its `qemu-system-*` process and matches it to the serial by its console port (`-port 5554` is `emulator-5554`). Every 30 seconds it reports that process's CPU (percent of one core) and resident memory, and the AVD name from its `-avd` argument. The process table is scanned when the agent starts and whenever adb's device list changes; in between, only the matched processes are read. A physical device has no emulator process, so its emulator readings are unavailable. When a flow fails on a device, the agent reads the device's resumed activity once (`dumpsys activity activities`) and sends its component as one extra device sample on the next sampling tick.

## Cost

`cargo xtask bench-overhead` measures the agent's cost on the captured four-flow suite the agent's tests replay, with a fake Maestro. It runs the suite with and without `fleet-agent run` (offline, no server), alternating the order, and reports wall time, the agent's CPU time and its peak memory. `--runs N` (default 5) sets the number of runs, `--linger SECS` (default 10) how long each run lasts, and `--max-cpu-percent X` makes it exit 1 when the agent uses more than X% of one core. The numbers depend on the machine, so the command is not part of `cargo xtask ci`.

Measured with `cargo xtask bench-overhead --runs 3 --linger 50` on an Apple M1 Pro (10 cores, macOS 26.7.1), release build, 50 s per run, about the length of the real four-flow suite. These figures predate the post-exit drain change in #128, so they have not been re-measured since; run the command again to refresh them:

- agent CPU: 99 ms per run (range 86 to 119 ms), 0.20% of one core and 0.02% of the host;
- agent peak memory: 17.2 MiB, supervisor and observer together;
- duration: the run with the agent took 0.32 s longer (range 0.27 to 0.34 s), 0.6% of 50 s. The cause was not isolated.

Shorter runs spend more of their time on the fixed start-up and exit cost: the default 10 s runs used 0.42% and 0.43% of one core in two runs on the same machine. Its own CPU and memory are also reported with every metrics sample.

## Signals and CI cancellation

On the first INT or TERM the agent forwards the signal to Maestro's process group and starts a grace timer (`FLEET_GRACE`); when it expires, the group is killed. Maestro itself shuts down in under a second (research 08). If the agent's supervisor dies, the observer terminates Maestro so no orphan JVM is left behind.

After Maestro exits, the agent keeps reading its output until the CI log has taken it. A pipe that is idle for 2 seconds is given up, and no pipe is waited on for more than 10 seconds after the exit, so a process that Maestro started and that keeps writing cannot hold the job open. Output a grandchild writes after that point is not forwarded to the CI log.

## Credentials

On GitHub Actions, give the job `permissions: id-token: write` and leave `FLEET_TOKEN` unset: the agent asks the runner for an OIDC token (audience `FLEET_OIDC_AUDIENCE`, default `fleet`) and exchanges it at `POST /api/v1/agent/token` for a short-lived agent token. The server decides the project and the trust level from the token's verified claims (repository and owner IDs, event, ref) and its trust policies; the job key comes from the verified `check_run_id`. Fork pull requests get no OIDC token and run offline.

Elsewhere, set `FLEET_TOKEN` to an API key (`fleet api-key create --project <slug> --name <ci>` on the server). Either way, a refused credential only turns telemetry off: Maestro runs and exits exactly as it would without the agent.

## Proxies and certificates

The agent trusts Mozilla's bundle, the system's certificate store and, with `FLEET_CA_FILE`, an extra PEM bundle (for TLS-inspecting corporate proxies). `wss://` connections go through `HTTPS_PROXY` (or `ALL_PROXY`), `ws://` through `HTTP_PROXY`, unless `NO_PROXY` matches the host (`host`, `.domain`, `domain` or `*`). The WebSocket opens an HTTP `CONNECT` tunnel; proxies are `http://`, optionally with `user:password@` (percent-encoded). The OIDC exchange uses the same proxies and certificates.

## The nightly emulator job

Fleet's own nightly job runs real Maestro against an emulator on the owner's Apple Silicon Mac, not on a GitHub-hosted runner: those have no GPU, and every Android 37 image crashed there. The job (`.github/workflows/nightly-emulator.yml`, cron `21 2 * * *`, 02:21 UTC) runs on the `fleet-mac` self-hosted runner and needs the Mac to:

- be awake and logged in at 02:21 UTC, with the runner online;
- be able to start Docker Desktop: the job runs `open -a Docker` when `docker info` fails and waits up to 3 minutes;
- have ports 80 and 443 free, for the reference Compose stack;
- have room for a throwaway emulator on port 5580 (serial `emulator-5580`, AVD `fleet-nightly`, kept under the runner's temporary directory). The owner's emulator on 5554 and its AVDs are not used.

An always-run step removes the emulator, its AVD and the Compose stack with its volumes. The image and GPU mode match the capture of the Phase 0 fixtures on this Mac.
