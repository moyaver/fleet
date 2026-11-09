# 13 assets: coordinate-mode benchmark

Throwaway measurement code and raw results for [research 13](../13-coordinate-mode.md) (issue #161). **Not product code.** Nothing here is built into a Fleet binary, and nothing here is tested by `cargo xtask ci`.

## Contents

| Path | What it is |
|---|---|
| `bench.py` | The benchmark. Modes: `shard-split`, `static`, `dynamic` (the prototype; `--batch N`), `serial`, `single`, plus `estimates` and `summary`. |
| `suite/` | The 12-flow suite: `config.yaml` and `f01.yaml` to `f12.yaml`. Each flow drives `com.fleet.anr`. `f12` fails on purpose. |
| `results/` | Raw results. Each run directory has `result.json` (pipeline time, device occupancy, every attempt, kills, correctness), `logs/` (Maestro stdout per process) and `junit/` (JUnit per process). `estimates.json`, `plan-2.json` and `matrix-2.json` are the planner inputs and outputs. |
| `results/cal-serial-3-discarded-device-dropped/` | The third calibration run, discarded: `emulator-5610` dropped out of adb during `f09`, and that run is not used. The run was repeated as `cal-serial-3`. Kept as evidence. |

## Reproduce

Needs: macOS (or Linux) with the Android SDK, the `google_atd` arm64 image for API 33, Maestro 2.12.1, Python 3.12, and a Rust toolchain for `fleet-planner`.

1. **The app.** The fixture is not on `main`. It is on branch `wave/4` at `fixtures/android/anr-app/`. Extract it and build with its own `build.sh`, which uses the installed SDK only:
   ```
   git archive wave/4 fixtures/android/anr-app | tar -x -C <scratch>
   <scratch>/fixtures/android/anr-app/build.sh <apk-out-dir>
   ```
   Install `anr-app.apk` on each emulator with `adb -s <serial> install -r`.
2. **Emulators.** Create two AVDs in a fresh `ANDROID_AVD_HOME`, start them headless with `-no-snapshot -gpu swiftshader_indirect` on distinct ports, and pass `-s <serial>` everywhere.
3. **Planner.** `cargo build -p fleet-planner --locked`, then:
   ```
   target/debug/fleet-planner plan --config research/13-assets/suite/config.yaml --shards 2 \
     --manifest <out>/plan.json --matrix <out>/matrix.json --estimates <out>/estimates.json
   ```
4. **Estimates.** Three `serial` runs on one device, then:
   `python3 research/13-assets/bench.py estimates --runs <run1> <run2> <run3> --out <out>/estimates.json`
5. **Modes.**
   - `python3 bench.py shard-split --devices A,B --out <dir>`
   - `python3 bench.py static --devices A,B --plan <plan.json> --out <dir>`
   - `python3 bench.py dynamic --devices A,B --estimates <estimates.json> [--batch 3] --out <dir>`
   - `python3 bench.py single --devices A --out <dir>` and `serial --devices A --out <dir>`
   - Add `--kill <serial> <seconds>` to `static` or `dynamic` to SIGKILL that device's Maestro process that many seconds into the run.
6. **Summary.** `python3 bench.py summary <dir> [<dir> ...]` prints the p50 and spread for one mode.

The script writes `result.json` in the run directory. The Maestro `--debug-output` folders (session logs, screenshots) are large, so they are not committed. They are in each run's `debug/` folder when the script runs.

## Caveats

- The suite's duration variance is designed (flow repeat counts 1 to 18). It is not a real customer suite. See research 13 §6.
- `estimates` uses the median of three calibration runs as stand-in history. Fleet has no history store in this study.
- The prototype has no device reset after a kill, and it does not tell infrastructure failures from test failures. See research 13 §2.4.
- Host load was high during the runs (1-minute load average 14 to 15). Per-run load is recorded in `loadavg_1min_at_end` from the second repetition onward.
