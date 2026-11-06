# Phase 0 flows

Flows used to capture the Maestro fixtures in `fixtures/maestro/` (PLAN §12 M1). They drive the
built-in Android **Settings** app, so they need no APK and the captures contain nothing private.
Written for an API 37 Google Play image; menu labels may differ on other images.

| File | Exercises |
|---|---|
| `pass.yaml` | a passing flow |
| `fail.yaml` | an assertion failure |
| `features.yaml` | nested `runFlow`, `retry`, `repeat`, optional (WARNED) and conditional (SKIPPED) steps, hooks, planted secrets |
| `retry-fail.yaml` | a `retry` block that runs out of attempts |
| `slow.yaml` | a long flow for signal and device-loss scenarios |
| `suite/` | a multi-flow suite (3 pass, 1 fail) for suite and shard runs |
| `ordered/` | `executionOrder` with `continueOnFailure: false` |
| `crash.yaml` | a wait that the capture breaks with `adb shell am crash com.android.settings` |
| `nested.yaml` | `runFlow` inside `runFlow` (`subflows/nested-outer.yaml`, `subflows/nested-inner.yaml`), a `retry` that fails once and then passes (inline `evalScript`), and a `repeat` around a `runFlow` |
| `screenshot.yaml` | two `takeScreenshot` steps |
| `recording.yaml` | `startRecording` … `stopRecording` |

The planted secrets (`fleet-planted-…`) are fake values; fixtures keep them on purpose so the
agent's redaction can be tested against real Maestro output.
