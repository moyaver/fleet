# Runner guide

## Install

This installs the alpha pre-release `v0.1.0-alpha.4` (pinned, because `latest` skips pre-releases). It is the release that carries the clean break in the [operator guide](operator-guide.md#clean-break-from-the-alpha-releases), so its agent reports to the server of the same release. To build the agent from source instead, run `cargo build --locked -p fleet-agent` and use `target/debug/fleet-agent` in place of the installed binary ([Try Fleet on a free VM](free-vm.md#7-connect-ci)).

The installer command for the release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/moyaver/fleet/releases/download/v0.1.0-alpha.4/fleet-agent-installer.sh | sh
```

Release archives are built for Linux x86_64 and aarch64 (static, musl) and macOS on Apple Silicon. Each comes with a SHA-256 checksum (also listed in `sha256.sum`), a GitHub artifact attestation, the license and the third-party notices. The checksum file is served from the same release as the archive, so a check against it proves only that the download was not corrupted. The attestation is what authenticates the archive. To verify a downloaded archive with the [GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify fleet-agent-x86_64-unknown-linux-musl.tar.xz --repo moyaver/fleet
```

## GitHub Actions

The [composite action](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent/action.yml) installs the agent and runs a Maestro command under it. Maestro's output and exit status are unchanged. Under the action, the agent appends a job summary to the run page. The summary gives the outcome counts, and the flows that did not pass with their reason and provenance. When `FLEET_SERVER` is set, each flow links to its flow page. The summary also links to the dashboard.

Before it runs anything, the action requires the release's build attestation to verify. This is the default (`verify-attestation: true`). The action takes the archive's expected SHA-256 from that attestation. The check needs the `gh` and `jq` CLIs on the runner, and a token with `attestations: read`. Set `verify-attestation: false` only where the archive cannot be authenticated anyway. The action then warns. Give the job `permissions: id-token: write` and `attestations: read`:

```yaml
- uses: moyaver/fleet/examples/github-actions/fleet-agent@v0.1.0-alpha.4
  with:
    command: maestro test .maestro/
```

The summary is included from `v0.1.0-alpha.2`; `v0.1.0-alpha.1` predates it. The example workflow is [`examples/github-actions/fleet-agent.yml`](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent.yml).

The `command` input splits words as a shell does, for quotes and backslashes. Nothing is expanded. For example, `maestro test "my flows/"` passes `my flows/` as one argument. An unterminated quote fails the step before Maestro starts. With `install: "false"`, the action runs the `fleet-agent` already on `PATH`, for example one built from the repository. It downloads nothing. The `summary-file` output names the file the summary was written to, for a later step that reads it.

The links open the flow page through `/api/v1/flows/{id}/page`. A browser without a signed-in dashboard session is sent to sign in, then back to the flow. A reader of another project gets the same not-found answer as for a flow that does not exist.

## Check the setup

```bash
fleet-agent check
```

It checks that `maestro` is on `PATH` (and which version), that the adb server is listening, that the Fleet server is reachable, and that the run directory is writable. Unlike `run`, it exits 1 on any problem.

## adb

The agent reads the device list from the adb server (`host:track-devices`) and never starts, restarts or kills the adb server: a different `adb` client version could restart it and break Maestro. If nothing listens on port 5037 (or `ANDROID_ADB_SERVER_PORT`), the agent waits.

Every 30 seconds the agent also probes each device that Maestro selected, with read-only adb shell commands (boot status, and CPU and available memory from `/proc`). The first probe of a device also reads its API level (`ro.build.version.sdk`). A probe has a 5-second timeout. A probe that fails or hangs leaves its readings unknown and never affects Maestro. Devices that Maestro did not select are not probed. While the host is under pressure (see [Cost](#cost)), the probe reads boot status only, and the guest CPU and memory stay unknown.

For an emulator, the agent finds its `qemu-system-*` process and matches it to the serial by its console port (`-port 5554` is `emulator-5554`). Every 30 seconds it reports that process's CPU (percent of one core) and resident memory, and the AVD name from its `-avd` argument. The process table is scanned when the agent starts and whenever adb's device list changes; in between, only the matched processes are read. A physical device has no emulator process, so its emulator readings are unavailable. When a flow fails on a device, the agent reads the device's resumed activity once (`dumpsys activity activities`) and sends its component as one extra device sample on the next sampling tick.

Every device sample carries the shard the device was assigned to: the planner's shard for a plan run (`fleet-agent run --plan … --shard N`), otherwise the shard number its `shard.assigned` event names. A reading the agent did not take is unknown and carries a reason, so the device page can say why: `not_an_emulator` (a physical device has no emulator process), `no_emulator_process` (no `qemu-system-*` process maps to the serial, or the device session is no longer driven), `no_avd_name` (the emulator's command line has no `-avd`), `probe_failed` (adb did not answer the probe or the API level read), `throttled` (the host is under pressure, see below), `no_baseline` (the first probe only sets the guest's CPU baseline, and so does the first probe after the guest's counters went backwards), `no_adb_list` (adb has not listed devices yet), and `not_sampled` (the sample does not take that reading: a probe carries no activity, and an activity read carries no probe). A reading that has a value has no reason.

A device session is the device as one shard of a run selected it. When a later shard selects the same serial, it is a new device session: the agent probes the newest one, reads its API level again and starts its CPU baseline over, so the old device's readings never mix with the new one's.

## Cost

`cargo xtask bench-overhead` measures the agent's cost on the captured four-flow suite that the agent's tests replay. The suite is a Maestro 2.12.1 capture, replayed here by a fake Maestro. It runs the suite with and without `fleet-agent run` (offline, no server), alternating the order, and reports wall time, the agent's CPU time and its peak memory. `--runs N` (default 5) sets the number of runs, `--linger SECS` (default 10) how long each run lasts, and `--max-cpu-percent X` makes it exit 1 when the agent uses more than X% of one core. The numbers depend on the machine, so the command is not part of `cargo xtask ci`.

Measured with `cargo xtask bench-overhead --runs 3 --linger 50` on an Apple M1 Pro (10 cores, macOS 26.7.1, release build of the agent at commit `6c27b31`), on 2026-11-09. Each figure is a median, with the range in brackets. The fake Maestro replays the captured Maestro 2.12.1 suite. No real Maestro run took part, so these figures are not a claim about a live run. The 50 s length is a chosen run length.

- agent CPU: 97.8 ms per run (73.2 to 105.0 ms), 0.19% of one core and 0.02% of the host;
- agent peak memory: 18.9 MiB (18.8 to 19.1 MiB), supervisor and observer together;
- duration: the run with the agent took 324.8 ms longer (309.3 to 344.1 ms), 0.6% of the 50 s run. The cause was not isolated.

The default 10 s runs (`--runs 5`) used 0.55% of one core (56.9 ms per run; 42.5 to 63.2 ms) and 19.0 MiB peak memory on the same machine. Shorter runs spend more of their time on the fixed start-up and exit cost. Its own CPU and memory are also reported with every metrics sample. The raw output of both runs, with the machine and the scope, is in [research 15 assets](../../research/15-assets/).

**Unknown host readings.** Every host and process reading the agent did not take carries its reason, the same as a device reading does: `no_system_info` (the platform's system information could not be read, so the host readings that come from it are not taken), `no_psi` (the kernel has no pressure stall information, as on macOS, so the three pressure readings are absent and never zero), `no_process` (Maestro has not started yet, or its process or the agent's has exited), `probe_failed` (the read failed), and `no_baseline` (the first CPU sample only sets the baseline). A reading that has a value has no reason. The newest host sample is on the runner page's *Newest host sample* card, with each reading's value or its reason. The units are part of each field's name and the scope is its prefix: `host_` is the machine, `runner_` the runner's process (Maestro today) and `agent_` the agent itself.

**Throttling.** Host and process metrics are sampled every 5 seconds by default. When the host is under pressure, the agent samples every 15 seconds by default, and skips the devices' guest CPU and memory reads, to take less from a busy runner. An operator can change both intervals (`ingest.sample_interval_ms` and `ingest.pressure_sample_interval_ms`; see the [operator guide](operator-guide.md)). The host counts as under pressure when Linux's pressure stall information (`/proc/pressure`) shows CPU `some avg10` at 40 or more, or memory `some avg10` at 20 or more, or when host CPU usage is 95 % or more; macOS has no pressure information, so there host CPU alone decides. These thresholds are defaults and are not configurable. The agent leaves the throttled state after three consecutive samples that are not under pressure, so it does not flap. While throttled, a device's guest CPU and memory are unknown, not zero, and their reason is `throttled`.

## Maestro's own screenshots and recordings

A flow's `takeScreenshot` images and `startRecording` videos are Maestro's own files, and their pixels are never redacted. The agent does not upload them unless you ask: pass `--upload-user-media` to `fleet-agent run`, or set `FLEET_UPLOAD_USER_MEDIA=1` (the same switch, for `run` and `publish` alike). With it on, a file is uploaded only when its flow failed, linked to the step that made it, and within the size cap the server sets for its class (`user_screenshot`, `user_recording`). A passing flow's files stay on the runner, and so does everything when `FLEET_UPLOAD_ARTIFACTS=off` is set. In an offline run the switch also puts a failed flow's media in `fleet-bundle.tar.zst`:

```sh
fleet-agent run --upload-user-media -- maestro test .maestro/
```

`fleet-agent publish --upload-user-media <output directory>` takes the same flag for a Maestro output directory, which it replays before uploading. A bundle already holds what its run decided, so the flag does not change a bundle.

## Signals and CI cancellation

On the first INT or TERM the agent forwards the signal to Maestro's process group and starts a grace timer (`FLEET_GRACE`); when it expires, the group is killed. In the Maestro 2.12.1 capture on macOS, shutdown on INT or TERM took under a second (research 08). If the agent's supervisor dies, the observer terminates Maestro so no orphan JVM is left behind.

After Maestro exits, the agent keeps reading its output until the CI log has taken it. A pipe that is idle for 2 seconds is given up, and no pipe is waited on for more than 10 seconds after the exit, so a process that Maestro started and that keeps writing cannot hold the job open. Output a grandchild writes after that point is not forwarded to the CI log.

## Credentials

On GitHub Actions, give the job `permissions: id-token: write` and leave `FLEET_TOKEN` unset. The agent asks the runner for an OIDC token (audience `FLEET_OIDC_AUDIENCE`, default `fleet`). It exchanges that token at `POST /api/v1/agent/token` for a short-lived agent token. The server decides the project and the trust level. It uses the token's verified claims (repository and owner IDs, event, ref) and its trust policies. The job key comes from the verified `check_run_id`. Fork pull requests get no OIDC token and run offline. With neither a `FLEET_TOKEN` (an empty one counts as none) nor an OIDC request, the agent connects to nothing and sends nothing. Maestro runs as it would without the agent.

Elsewhere, set `FLEET_TOKEN` to an API key (`fleet api-key create --project <slug> --name <ci>` on the server). Either way, a refused credential only turns telemetry off: Maestro runs and exits exactly as it would without the agent.

## Offline runs and `publish`

A run with no `FLEET_SERVER`, such as a fork pull request, is offline. It keeps everything locally. When it ends, it writes `fleet-bundle.tar.zst` into its run directory (`FLEET_RUN_DIR`, else `$RUNNER_TEMP/fleet-agent/<id>`). The bundle holds the run's journal (every event, as `journal.jsonl` records them). It also holds each artifact the run would have uploaded: failure screenshots, `commands.json`, the hierarchy, the device log excerpt, the JUnit report, `maestro.log` and the HTML report. Each artifact is redacted exactly as a live upload is. The bundle never holds a value the server would not have seen (I6). The bundle is written after Maestro has exited, and it never changes Maestro's output or exit status. If it cannot be written, the run prints one notice and is otherwise unchanged.

Keep the bundle as a workflow artifact, then send it from a job that can reach the server:

```bash
FLEET_SERVER=https://fleet.example.com FLEET_TOKEN="$FLEET_TOKEN" fleet-agent publish fleet-bundle.tar.zst
```

`publish` also takes a Maestro output directory, such as a capture or a saved debug directory like `…/artifacts`. It replays that directory through the same session a live run uses, and uploads its files the same way. Either way it needs `FLEET_SERVER` and `FLEET_TOKEN`, or the CI OIDC variables, as a live run does. It prints the run it created. The run gets a new agent run id. Its CI identity is the bundle's, or for a directory, the publishing job's. A directory with no `meta.json` takes its times from the files it holds. It uses the `maestro.log` records and the flows' `commands.json` timestamps. It uses their modification times only when neither has one. The run keeps the day it started, not the day it is published, so it is filed under the day it ran.

A published run is imported, not live. The dashboard labels it *Published* on its pipeline, flow, history and flow pages, and the REST API says so with `origin: "published"` on its agent run, its flows and its history entries (a live run says `"live"`). Its events keep their evidence kinds, except that live console output is recorded as debug-log evidence, because after the fact it is no longer live (I4). To read a bundle without publishing it, extract it (`tar --zstd -xf fleet-bundle.tar.zst -C dir`) and run `fleet-agent inspect dir`, which reports the journal's events and whether it is complete. A bundle whose journal reached its size cap says so, and the published run carries a `journal.gap` for the events the bundle does not hold; the run is not ended in that case.

The server keeps a journal only as long as its retention (`retention.journal_days`, 14 by default). A run that started before that is refused with a reason, rather than accepted and removed by the next retention pass: publish it sooner, or raise the retention.

## Proxies and certificates

The agent trusts Mozilla's bundle, the system's certificate store and, with `FLEET_CA_FILE`, an extra PEM bundle (for TLS-inspecting corporate proxies). `wss://` connections go through `HTTPS_PROXY` (or `ALL_PROXY`), `ws://` through `HTTP_PROXY`, unless `NO_PROXY` matches the host (`host`, `.domain`, `domain` or `*`). The WebSocket opens an HTTP `CONNECT` tunnel; proxies are `http://`, optionally with `user:password@` (percent-encoded). The OIDC exchange uses the same proxies and certificates.

## The nightly emulator job

Fleet's own nightly job runs real Maestro against an emulator on the owner's Apple Silicon Mac, not on a GitHub-hosted runner: those have no GPU, and every Android 37 image crashed there. The job (`.github/workflows/nightly-emulator.yml`, cron `21 2 * * *`, 02:21 UTC) runs on the `fleet-mac` self-hosted runner and needs the Mac to:

- be awake and logged in at 02:21 UTC, with the runner online;
- be able to start Docker Desktop: the job runs `open -a Docker` when `docker info` fails and waits up to 3 minutes;
- have ports 80 and 443 free, for the reference Compose stack;
- have room for a throwaway emulator on port 5580 (serial `emulator-5580`, AVD `fleet-nightly`, kept under the runner's temporary directory). The owner's emulator on 5554 and its AVDs are not used.

An always-run step removes the emulator, its AVD and the Compose stack with its volumes. The image and GPU mode match the capture of the Phase 0 fixtures on this Mac.

## Hardening a runner

Fleet's CI jobs run user code on the runner, so a runner that outlives one job is a standing target. The steps below are the ones the repository can document. The repository cannot enforce them: they are GitHub or infrastructure settings (research 06 T18, T19).

- **No fork pull requests on a self-hosted runner.** The CI workflows already run fork pull requests on hosted runners only. Keep the runner's labels out of any workflow that a fork can trigger, and require approval for fork pull request workflows (repository Actions settings).
- **Ephemeral or just-in-time registration.** Register the runner with `./config.sh --ephemeral`, or through a JIT configuration, so each job gets a fresh runner that is removed afterwards. A persistent runner keeps its work directory, and anything a job left there is available to the next job.
- **A separate runner group** for Fleet-instrumented jobs, with access to this repository only.
- **An unprivileged user** runs the runner service. It has no `sudo`, no Docker group membership it does not need, and no access to other users' keys.
- **Read-only default token.** Set the repository's default workflow permissions to read. Grant `id-token: write` and `attestations: read` per job, as the examples do.
- **OIDC, not stored secrets.** Authenticate the agent with the job's OIDC token (`id-token: write`). A static `FLEET_TOKEN` is for push and schedule jobs only, stored as an environment secret with protection rules.
- **Pinned actions.** Pin every third-party action by its full commit SHA. The examples pin Fleet's composite action to a release tag; move that tag with each release.

**Left for the owner (not in the repository):**

1. Turn on **immutable releases** for `moyaver/fleet` (repository settings, Releases), so a published release tag and its assets cannot be changed.
2. Run the `fleet-mac` self-hosted runner **ephemerally or just-in-time** (runner registration on the Mac; see the steps above).
