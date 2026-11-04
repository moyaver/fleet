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

## Proxies and certificates

The agent trusts the system's certificate store and Mozilla's bundle. Proxy support (`HTTPS_PROXY`) is not in this release yet.
