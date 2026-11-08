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
| `paths-debug-and-test-output-apart` | | `--device A test --debug-output {RUN}/D --test-output-dir {RUN}/T pass.yaml` |
| `paths-flatten-alone` | `user.home` is `{RUN}/home` (see [provenance](#provenance-of-the-layout-captures-148)) | `--device A test --flatten-debug-output pass.yaml` |
| `paths-flatten-test-output-dir` | `user.home` is `{RUN}/home` | `--device A test --flatten-debug-output --test-output-dir {RUN}/T pass.yaml` |
| `paths-flatten-debug-and-test-output` | | `--device A test --flatten-debug-output --debug-output {RUN}/D --test-output-dir {RUN}/T pass.yaml` |
| `paths-flatten-repeated` | exits 2 before it runs anything | `--device A test --flatten-debug-output --flatten-debug-output --debug-output {RUN}/D pass.yaml` |
| `paths-flatten-config` | `user.home` is `{RUN}/home`; run from `{RUN}`, with `cfg/config.yaml` | `--device A test --flatten-debug-output --config cfg/config.yaml pass.yaml` |
| `paths-config-explicit` | run from `{RUN}`, with `cfg/config.yaml` (`testOutputDir: out-config`) | `--device A test --config cfg/config.yaml pass.yaml` |
| `paths-config-overridden-by-flag` | as `paths-config-explicit` | `--device A test --config cfg/config.yaml --test-output-dir {RUN}/T pass.yaml` |
| `paths-config-dir-input` | run from `{RUN}`, with `flows/config.yaml` beside `flows/pass.yaml` | `--device A test flows/` |
| `paths-config-file-beside` | as `paths-config-dir-input` | `--device A test flows/pass.yaml` |
| `tty-ansi`, `tty-no-ansi` | (run under `script -q /dev/null`) | `[--no-ansi] --device A test CANON fail.yaml` |
| `suite-with-adb-load` | `MP`, plus an adb poller every 5 s | `--device A test CANON suite` |
| `shard-split-2` | `MP` | `--device "A,B" test --shard-split 2 CANON suite` |
| `shard-all-2` | `MP` | `--device "A,B" test --shard-all 2 CANON suite` |
| `sigint`, `sigterm`, `sigkill` | `MP --signal INT\|TERM\|KILL --signal-after-ms 30000` | `--device A test CANON slow.yaml` |
| `device-killed` | `MP`, plus `adb -s B emu kill` after 35 s | `--device B test CANON slow.yaml` |
| `html-report` | `MP` | `--device A test --format html --output {RUN}/report.html --flatten-debug-output --debug-output {RUN}/debug pass.yaml` |
| `html-report-default-path` | `MP`, run from `{RUN}` (see [provenance](#provenance-of-the-html-scenarios-43)) | `--device A test --format html --flatten-debug-output --debug-output {RUN}/debug <REPO>/examples/flows/phase0/pass.yaml` |
| `html-detailed` | `MP`, `--format html-detailed --output {RUN}/report.html` | `--device A test --format html-detailed --output {RUN}/report.html --flatten-debug-output --debug-output {RUN}/debug fail.yaml` |
| `html-detailed-default-path` | `MP`, run from `{RUN}` | `--device A test --format html-detailed --flatten-debug-output --debug-output {RUN}/debug <REPO>/examples/flows/phase0/pass.yaml` |
| `html-test-output-dir` | `MP`, run from `{RUN}` | `--device A test --format html --test-output-dir {RUN}/T <REPO>/examples/flows/phase0/fail.yaml` |
| `html-no-debug-flags` | `MP`, run from `{RUN}` | `--device A test --format html <REPO>/examples/flows/phase0/fail.yaml` |

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

### Provenance of the HTML scenarios (#43)

Captured on 2026-11-08 on macOS 26 (Apple Silicon) with the Homebrew Maestro 2.12.1 formula, on a throwaway AVD `fleet-fixtures-43` created in a temporary `ANDROID_AVD_HOME` from the installed `google_apis_playstore` arm64 image (Android 17, build `CE2A.260420.051.A2`, API 37). It booted headless on port 5584 (serial `emulator-5584`), not 5586: adb kept the emulator offline for nine minutes on 5586, whose adb port (5587) is outside adb's recommended range. The AVD was deleted afterwards and the emulator was shut down; the owner's emulators were not used. `device.json` in each scenario records the device, the AVD configuration and these notes. "Run from `{RUN}`" means Maestro is started through `sh -c 'cd "$1" && shift && exec "$@"'` with `{RUN}` as its working directory, so a report written there lands in `run/`. Every scenario sets `MAESTRO_CLI_LOG_PATTERN_FILE` (`MP`). The flows did not pass on this image: `pass.yaml` failed on "Network & internet" or "Apps" in most runs, and the device's system server died during `html-detailed` (its `logs/crash-report.txt` is the Settings crash). Each `exit.json` and `stdout.bin` records the outcome as Maestro reported it, and the reports say FAILED.

Findings:

- **Where the report goes without `--output`:** Maestro 2.12.1 writes `report.html` in its working directory, for both `--format html` and `--format html-detailed`. It is one file, with no assets directory (`html-report-default-path`, `html-detailed-default-path`). It is not written into `--debug-output`, into `--test-output-dir` (`html-test-output-dir`: the flow bundles go into `T/<timestamp>/`, the report stays in the working directory), or into the session state directory (`html-no-debug-flags`: the session tree is `state/maestro/tests/<timestamp>/`).
- **With `--output`:** the report is written to that path exactly, whatever the format; `html-report` and `html-detailed` hold it at `run/report.html`.
- **Imports:** every report is well under the import limit (2.3 KB to 5.9 KB), so no `SKIPPED.txt` was needed. `device-logcat.txt` is cut to 32 KiB as usual.

### Provenance of the layout captures (#148)

Captured on 2026-11-08 on macOS 26 (Apple Silicon) with the Homebrew Maestro 2.12.1 formula, on a throwaway AVD `fleet-fixtures-148` created in a temporary `ANDROID_AVD_HOME` from `system-images;android-37.0;google_atd;arm64-v8a` (the image on the capturing machine). It booted headless on port 5584 (serial `emulator-5584`) and was shut down and deleted afterwards. `device.json` in each scenario records the device, which is the same for all of them. Every scenario ran `pass.yaml` on this image, and it failed (exit 1) there: the layouts do not depend on the outcome, and each `exit.json` records what Maestro reported.

- **`user.home`.** The JVM does not read `$HOME`: with `HOME` changed, `java -XshowSettings:properties` still reports the account's home (checked on macOS). The flattened captures that write into the home directory run through `sh -c 'h="$1"; …; export JAVA_OPTS="-Duser.home=$h"; exec "$@"'` with `{RUN}/home`, which Maestro's launcher passes to the JVM. The fixture records the wrapper in `meta.json`. Files Maestro wrote beside the flow bundles there (the adb key it created, the simulator tools under `.maestro/deps`, `.maestro/sessions`, the DebugLogStore zip under `Library/Logs`) were removed on import: the adb key is a private key, and none of the rest is part of any layout.
- **Flatten alone** writes into `user.home`: `home/maestro.log` and `home/pass/`.
- **Flatten with `--test-output-dir`** ignores `T`: the bundles stay in `home/`, and no `T` directory is created.
- **Flatten with a debug and a test directory** ignores `T`: everything is in `D/`.
- **A repeated `--flatten-debug-output`** is a picocli usage error: `option '--flatten-debug-output' should be specified only once`, exit 2, and nothing runs.
- **`testOutputDir` in a workspace config** is read from `--config`, and from a `config.yaml` (or `config.yml`) in a directory the command names, not from a single file beside it (`paths-config-file-beside` writes its bundles beside the log). Its value is relative to the working directory, not to the config file (`paths-config-explicit` writes to `run/out-config/`, not `run/cfg/out-config/`). `--test-output-dir` wins over it (`paths-config-overridden-by-flag`), and `--flatten-debug-output` ignores it (`paths-flatten-config`).
- **Two runs in one tree** are told apart by the process that holds each run's `maestro.log` open: Maestro's log4j file appender keeps the file open for the JVM's life (checked by running Maestro's own `LogConfig` in a JVM and listing its open files with `lsof`). The agent's tests use this to tell two concurrent runs apart (`crates/fleet-agent/tests/it/layouts.rs`).

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
