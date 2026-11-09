# 15: Setup cost, from nothing to a first run on the dashboard: measured

**Research date:** 2026-11-09
**Issue:** [#146](https://github.com/moyaver/fleet/issues/146)
**Builds on:** [10 Comparative diagnosis study](10-comparative-diagnosis-study.md), which marked setup cost as not measured.
**Scope:** how long it takes a new user to get from nothing to a Maestro run that shows on the Fleet dashboard, following only the published docs (`docs/src/free-vm.md`, `docs/src/operator-guide.md`, `docs/src/runner-guide.md`, `docs/src/user-guide.md`, `deploy/compose/README.md`, `examples/github-actions`). Raw logs: `scratchpad/setup146/` (not in the repository).

## What was timed, and what was not

This is not a VM run. There was no fresh VM, and a paid one was out of scope. The closest honest stand-in was used for each part:

- **Server (free-vm.md steps 5 to 7).** A fresh clone of the branch in a scratch directory, a unique Compose project name (`-p fleet-setup146`), and the server built from that clone on a Mac (arm64 Linux containers under Docker Desktop, 10 cores). `FLEET_DOMAIN=localhost`, so Caddy signs with its own CA. The published alpha image was not pulled because it predates the clean break (operator guide, "Clean break from the alpha releases"); this is the reason the docs now say to build.
- **Agent (runner-guide and user-guide).** A debug `fleet-agent` built from the clone, wrapping `maestro test` on a throwaway Android emulator (`google_atd`, API 33, arm64, port 5620, `-gpu swiftshader_indirect`) against that local server.
- **GitHub Actions.** Read-only `gh` data from recent runs of the CI job `fleet-agent action (install, run and job summary)` (`.github/workflows/ci.yml`). That job runs the composite action for real, with a fake Maestro. A GitHub-hosted run cannot reach a server on this Mac, so no CI run reached the local dashboard.

**Not timed, and why:**

- VM provisioning, the Docker Engine install, the ports, the two DuckDNS names, and public TLS (`free-vm.md` steps 1 to 4). None of them was run.
- A GitHub-hosted job that sends its first run to this server. The local server is not reachable from GitHub runners.
- A real app's Maestro suite. The flows were the fixture app and one Settings flow (see friction 8).

## Results

Times are wall-clock, from `date +%s` logged before and after each step (`scratchpad/setup146/run1-compose.log`, `run1-emu.log`, `run2-compose.log`, `run2-emu.log`). Load is the 1-minute load average logged with each step; it was between 1 and 4 while the Docker build ran and between 5 and 22 during the emulator and Maestro work on the shared host.

| | Run 1 (before the fixes) | Run 2 (after the fixes, fresh clone) |
|---|---|---|
| Server: clone to healthy stack, CA, `/readyz` 200, anonymous read, API key | 9 min 17 s (clone 10:34:42 to key 10:43:59 + readyz) | 7 min 15 s (clone 10:52:01 to key 10:59:16) |
| of which: `docker compose up -d --build` | **8 min 17 s** (10:34:56 to 10:43:13) | 6 min 54 s (10:52:06 to 10:59:00) with a partly cached build (see below) |
| Agent: `cargo build --locked -p fleet-agent`, debug, fresh clone | 30.49 s | 34.71 s |
| Emulator: write AVD config and boot, `boot_completed=1` | about 31 s after launch (launch 10:45:57, after one failed start) | 24 s after launch (launch 11:00:05) |
| `fleet-agent check` (maestro, adb, server over TLS, run dir) | 3 s | 3 s |
| First run on the dashboard | the failed `pass.yaml` run at 12 min 15 s after the clone (see friction 8) | the passing run at 8 min 41 s after the clone |
| Passing flow through `fleet-agent run --` | 10 s (exit 0) | 11 s (exit 0) |
| Dashboard check over HTTP (attempts list and detail) | returns the run, state `passed`, `open_jobs: 0` | same |

**The first run appears when the agent starts.** In both runs the server recorded the attempt (`first_seen_at`) within one second of `fleet-agent run` starting, so the run is on the dashboard while Maestro is still running. The dashboard shows the flow's outcome when it ends.

**Build cache, and how to read the two server times.** Run 1 was not logged per Docker step, so its build cache state is not known. Run 1 is the figure to use as the setup time: it is the build a new user would see on a Mac with the Rust base image and some earlier layers present. Run 2 used the same Dockerfile after the docs-only commits. Its per-step log (`scratchpad/setup146/run2-up.out`) shows steps 11 (tool install), 12, 13, 16 (copy recipe) and 17 (dependency cook) as `CACHED`. Steps 18 to 20 (`COPY . .`, the dashboard build with trunk, the server compile) were rebuilt, because the context changed. So the 6 min 54 s of run 2 is a cached build and is **not** a setup time. The non-build part of run 2 is the measure to compare: 5 s from clone to secrets, and 16 s from the end of the build to a healthy stack, CA, `/readyz`, the `[api]` change and the key.

**Step counts.** Counted by hand from the logs, as commands the user runs (a block of Start-section commands counts once):

| | Run 1 | Run 2 (after the fixes) |
|---|---|---|
| Server, clone to API key | 14 (including the `[api]` `curl` check and two extra fixes made by hand) | 9 |
| Agent, documented steps (cargo build, check, flow, dashboard check) | 4 | 4 |
| Agent, undocumented steps (emulator AVD config, emulator start and boot, fixture build and install) | 9 | 6 |
| Passing run reached | after 2 flows (one failed on the Settings app) | first flow |

The repeat's undocumented emulator steps are the same hand-written AVD workaround (friction 9, environment): the docs do not cover an emulator at all, so the runner guide's "a working Android emulator" is a prerequisite a user must already have.

**GitHub Actions, the documented path.** The CI job `fleet-agent action (install, run and job summary)` ran 8 times on recent successful runs (42518976856 to 42563232885). Its wall time was 55 to 76 s. The step times were:

- the composite action's install step (download, `gh attestation verify`, extract): 4 to 5 s;
- `Build the repository's agent` (`cargo build --locked -p fleet-agent` on `ubuntu-latest`): 45 to 64 s;
- the run step on the fake Maestro: 1 to 2 s.

The second half of that job is the source-build path that `free-vm.md` step 7 now documents. A user's workflow needs the steps in that YAML: checkout of the app, checkout of this repository, the build, the `PATH` line, and the composite action with `install: "false"` (5 steps), plus `permissions: id-token: write` and `attestations: read` and `FLEET_SERVER` in the job. With the release installer the composite action needs one step less (no build), but that installs the alpha.3 agent, which cannot report to a server built from this branch.

## Friction found, and what was fixed

Found by following the docs (run 1), each fixed in its own commit on `docs/setup-cost`:

1. **The secret count.** `free-vm.md` step 5 said to create "the four files in `secrets/`". The Start section creates eight, including `garage.env`, which Compose needs before it starts. Fixed in `a6f140e`.
2. **Garage keys in `.env`.** The same step told the user to put `GARAGE_RPC_SECRET`, `GARAGE_ACCESS_KEY` and `GARAGE_SECRET_KEY` in `.env` with a `printf`. `.env.example` has no such keys, and Compose reads the Garage key pair from secret files, so those entries did nothing. Fixed in `a6f140e`.
3. **The `[api]` table.** Step 6 said to set `anonymous_read` "under `[api]`", but the reference `fleet.toml` has no `[api]` table. The fix names the file and gives the two lines. Fixed in `41a9491`.
4. **The CA step.** `free-vm.md` checks `/readyz` with a public name only. Without public DNS (`FLEET_DOMAIN=localhost`), Caddy signs with its own CA, and the agent needs that file through `FLEET_CA_FILE`. Only the Compose guide had the export. Fixed in `f36ac3e`.
5. **The published server image cannot serve this branch.** `free-vm.md` and the Compose guide said to pull `ghcr.io/moyaver/fleet:0.1.0-alpha.3` and to leave out `--build`. That image predates the clean break, so an agent built from this branch would not be read. The docs now say, until the next release, to build the server from the clone with `FLEET_IMAGE=fleet:source` and `docker compose up -d --build`. Fixed in `601dbe6`. The defaults in `.env.example` and `docker-compose.yml` still say alpha.3; the release that bumps them should change them (see Decisions).
6. **No way to get an agent.** Step 7 said to wrap the Maestro command with `fleet-agent run --` but did not say how to install the agent. The installer and the composite action both fetch alpha.3, for the same reason as friction 5. Fixed in `d732763`: the source build, and the composite action run with `install: "false"` from this repository's own CI pattern.
7. **The runner guide's install section.** It presented the alpha.3 installer as the install path. Fixed in `d732763`: the note above the installer command says it cannot talk to this branch's server, and gives the source build.
8. **The phase 0 flows need a Settings app.** The flows whose `appId` is `com.android.settings` fail on the Automated Test Device image (`google_atd`) with `Package com.android.settings is not installed`, because that image has no Settings package. The flow README said the flows were written for a Google Play image but not what the other image does. Fixed in `342cee1`. The ANR flows need the fixture app from `fixtures/android/anr-app`, and the run used it.

Not fixed, because it is an environment problem and no doc owns it: see the limits below.

## Limits (environment, not Fleet docs)

- **`avdmanager` does not run on the JDKs installed on this Mac.** It fails with `NoClassDefFoundError: javax/xml/bind/annotation/XmlSchema` under JDK 27 (Homebrew) and under JDK 21 (temurin). The AVD was written by hand instead: an `.ini` and a `config.ini` in a temporary `ANDROID_AVD_HOME`, using the image at `system-images/android-33/google_atd/arm64-v8a`. A user on another machine may not hit this.
- **A hand-written AVD needs `hw.cpu.arch=arm64`.** Without it the emulator refuses to start (`CPU Architecture 'arm' is not supported`). `avdmanager` would have written it.
- **Emulator crash data.** The emulator wrote its crash database to `/tmp/android-mohamadjaber`. That path is the emulator's own default and was left alone.
- **Docs do not cover an emulator.** The runner guide assumes "a working Android emulator or device" and does not say how to make one. Both timings above include only the boot, not AVD creation by a user who has the tools.
- **Only one device was attached.** `--device emulator-5620` was passed to Maestro, so the agent never selected another device. The owner's emulators were not running during the runs (`adb devices` was empty), and none were started, changed or deleted.

## Decisions

- **The defaults stay for the release.** `deploy/compose/.env.example`, `deploy/compose/docker-compose.yml` (`FLEET_IMAGE` default) and `examples/github-actions/fleet-agent/action.yml` (`version` default) still name `v0.1.0-alpha.3`. They are correct for the released alpha and must change when the release carrying the clean break is cut. No release was invented here.
- **The docs say the build path plainly** until that release, in the free VM guide, the Compose guide, the runner guide and the README.

## Not timed on a VM, with the docs' claims

The docs claim these steps but were not run here: `free-vm.md` steps 1 to 4 (the Oracle Always Free shape, the ports and the iptables rules, Docker's apt repository, the group membership), and the public-name certificate from ACME. Those costs are for the user's own provider and were not measured. The VM's own cost, in addition to the numbers above, is therefore unknown.
