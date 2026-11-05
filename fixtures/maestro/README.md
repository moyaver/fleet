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
