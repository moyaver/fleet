# Maestro fixtures

Real Maestro runs, captured on an emulator, used as parser and agent test inputs. **Never edit by hand and never invent one** (PLAN §0.6): capture, then import.

Layout: `<maestro version>/<scenario>/`

| File | What it is |
|---|---|
| `stdout.bin`, `stderr.bin` | Maestro's output, byte for byte |
| `events.jsonl` | one timeline, ms since start: `stdout`/`stderr` lines, `fs` (file created / grew), `log` (each `maestro.log` line as a tailer saw it), `proc` (`ps` snapshots), `signal` |
| `exit.json` | exit code or signal, duration |
| `meta.json` | the exact command and environment |
| `run/` | what Maestro wrote: `debug/` (session `maestro.log`, flow bundles) and `report.xml` |
| `state/` | `$XDG_STATE_HOME/maestro` (Maestro's own state) |
| `device.json` | Maestro version, device, API and emulator setup (scenarios captured for #77 on) |

Paths are normalized to `<REPO>`, `<CAPTURE>`, `<MAESTRO_HOME>`, `<HOME>` and `<USER>`; device logcat is cut to 32 KiB. The values `fleet-planted-…` are fake secrets planted on purpose, so redaction can be tested against real output.

Findings from these fixtures: [research 08](../../research/08-phase0-results.md).

## Scenarios (2.12.1)

Flows are in [`examples/flows/phase0/`](../../examples/flows/phase0/). `A`/`B` are two emulators of the same AVD; `CANON` is `--format junit --output {RUN}/report.xml --flatten-debug-output --debug-output {RUN}/debug`; `MP` is `--env MAESTRO_CLI_LOG_PATTERN_FILE='MFLT1|%d{UNIX_MILLIS}|%t|%level|%logger|%enc{%msg}{CRLF}%n'`.

| Scenario | Capture options | Maestro arguments |
|---|---|---|
| `pass` | | `--device A test CANON pass.yaml` |
| `absolute-path` | captured from outside the checkout with timeline and flow bundles; checkout normalized to `/fixture/checkout` | `--device A --no-ansi test CANON /fixture/checkout/examples/flows/phase0/pass.yaml` |
| `pass-machine-log` | `MP` | `--device A test CANON pass.yaml` |
| `fail` | `MP` | `--device A test CANON fail.yaml` |
| `suite` | `MP` | `--device A test CANON suite` |
| `features` | `MP --env MAESTRO_FLEET_TOKEN=fleet-planted-maestro-secret-91c2` | `--device A test CANON -e PASSWORD=fleet-planted-e-secret-7f3a features.yaml` |
| `retry-fail` | `MP` | `--device A test CANON retry-fail.yaml` |
| `execution-order` | `MP` | `--device A test CANON ordered` |
| `crash` | `MP`, under `sh -c` (see [provenance](#provenance-of-the-2121-scenarios-added-for-77)) | `--device emulator-5582 test CANON crash.yaml` |
| `nested` | `MP` | `--device emulator-5582 test CANON nested.yaml` |
| `screenshot` | `MP` | `--device emulator-5582 test CANON screenshot.yaml` |
| `recording` | `MP` | `--device emulator-5582 test CANON recording.yaml` |
| `single-flow-no-format` | `MP` | `--device A test pass.yaml` |
| `verbose` | `MP` | `--verbose --device A test CANON pass.yaml` |
| `bad-flag` | | `--device A test --not-a-real-flag pass.yaml` |
| `no-device` | `MP` | `--device emulator-5999 test CANON pass.yaml` |
| `paths-none` | | `--device A test pass.yaml` |
| `paths-test-output-dir` | | `--device A test --test-output-dir {RUN}/T pass.yaml` |
| `paths-debug-output` | | `--device A test --debug-output {RUN}/D pass.yaml` |
| `paths-debug-and-test-output-same` | | `--device A test --debug-output {RUN}/X --test-output-dir {RUN}/X pass.yaml` |
| `tty-ansi`, `tty-no-ansi` | (run under `script -q /dev/null`) | `[--no-ansi] --device A test CANON fail.yaml` |
| `suite-with-adb-load` | `MP`, plus an adb poller every 5 s | `--device A test CANON suite` |
| `shard-split-2` | `MP` | `--device "A,B" test --shard-split 2 CANON suite` |
| `shard-all-2` | `MP` | `--device "A,B" test --shard-all 2 CANON suite` |
| `sigint`, `sigterm`, `sigkill` | `MP --signal INT\|TERM\|KILL --signal-after-ms 30000` | `--device A test CANON slow.yaml` |
| `device-killed` | `MP`, plus `adb -s B emu kill` after 35 s | `--device B test CANON slow.yaml` |

### Provenance of the 2.12.1 scenarios added for #77

Captured on 2026-11-07 on macOS 26 (Apple Silicon) with the Homebrew Maestro 2.12.1 formula (`/opt/homebrew/Cellar/maestro/2.12.1`). The device was a throwaway AVD set up the way the nightly job does it (`.github/workflows/nightly-emulator.yml`): `system-images;android-37.1;google_apis_playstore_ps16k;arm64-v8a` (Android 17, build `CP31.260623.012`), `hw.gpu.enabled=yes`, `hw.gpu.mode=auto`, 1080x2424 at 420 dpi, 3 GB RAM, booted headless on port 5582 (serial `emulator-5582`, AVD `fleet-fixtures-77`). The AVD lived in a temporary `ANDROID_AVD_HOME` and was deleted afterwards. The owner's emulator was not used. `device.json` in each scenario records these values.

Imports normalize paths and cut `device-logcat.txt` to 32 KiB head and tail, and files over 1 MiB are listed in `SKIPPED.txt`.

- **`crash`**: a real app crash during a wait. `crash.yaml` launches Settings, asserts "Network & internet", then waits 20 s for text that never shows. The capture's `sh -c` loop waits until `com.android.settings` has a pid, sleeps 4 s, then runs `adb -s emulator-5582 shell am crash com.android.settings`. Settings dies with `RemoteServiceException$CrashedByAdbException: shell-induced crash` and the flow fails with `Assertion is false: "Fleet never shows this" is visible` after 22 s. Maestro wrote `logs/crash-report.txt`. The import cut the middle of `logs/device-logcat.txt`, so `FATAL EXCEPTION` is not in the fixture. The 1.25 MB failure screenshot is over the import limit and is in `SKIPPED.txt`.
- **`nested`**: `nested.yaml` runs `subflows/nested-outer.yaml`, which has `runFlow` (inside it, `nested-inner.yaml`), then `retry` (2 retries) around an inline `evalScript` and `assertVisible: "${output.target}"`, then `repeat` (2) around `runFlow: nested-inner.yaml`. The `evalScript` aims at text that is absent on attempt 1 and present on attempt 2, so the log shows `Attempt 1 failed for retry command` and the flow passes. `features` already covers one-level `runFlow`, a `retry` that passes first time and `repeat`, so it is not repeated. `retry-fail` covers an exhausted retry. The target is changed rather than thrown because a `runScript` that throws was not retried by Maestro in a trial run. Repo rules forbid `.js` files (invariant I9), so there is no script file.
- **`screenshot`**: two `takeScreenshot` steps. Maestro writes `debug/<flow>/takeScreenshot/<name>.png`, not into the working directory.
- **`recording`**: `startRecording: fleet-recording` … `stopRecording`. Maestro writes `debug/<flow>/startRecording/fleet-recording.mp4` (408 KB, kept).
- **`anr`: not captured.** Tried, none reproduced an app ANR:
  - `adb shell am hang`: hangs the whole system (`Watchdog: WAITED_UNTIL_PRE_WATCHDOG`). It is not an app ANR.
  - `adb shell am freeze com.android.settings` with Settings in the foreground, then `input tap`: Settings died (`ActivityTaskManager: Force removing ActivityRecord … app died`). No "isn't responding" dialog and no `am_anr` event.
  - Stopping the process with `SIGSTOP`: needs root, and `adb root` on this `google_apis_playstore` image returns `adbd cannot run as root in production builds`.
  - A reproducible ANR needs an app whose main thread blocks, such as a test APK that sleeps in a broadcast receiver or in `onCreate`. Adding such an app to the repo is the owner's call.

## Capturing a new version

```bash
cargo xtask fixtures capture --out /tmp/cap/pass -- maestro --device emulator-5554 test --format junit --output {RUN}/report.xml --flatten-debug-output --debug-output {RUN}/debug examples/flows/phase0/pass.yaml
```

```bash
cargo xtask fixtures import /tmp/cap/pass 2.13.0/pass
```

A new Maestro minor needs fixtures before Fleet claims support for it (PLAN §13).

Older reports that contain absolute paths without a known checkout root cannot be safely
merged into repository-relative history; Fleet leaves their identity unresolved.
