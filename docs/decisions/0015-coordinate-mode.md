# ADR-0015: Coordinate mode is not built; observe mode and the static planner stay

**Status:** Accepted (rejected under the owner's 20% rule) · **Date:** 2026-11-09 · **Issue:** [#161](https://github.com/moyaver/fleet/issues/161) · **Research:** [13](../../research/13-coordinate-mode.md)

## Context
Fleet's v1 is observe mode plus the static planner (ADR-0013). Spec §17 rules out "CI orchestration or job scheduling", and ADR-0013 keeps a dynamic work queue out of v1 "unless measurements show static sharding falls short". Issue #161 asked whether an optional coordinate mode is worth building: CI starts workers, and Fleet hands each Maestro flow to whichever worker is free.

The owner set the threshold before measuring: coordinate mode is worth building only if a dynamic prototype gives at least **20% lower p50 pipeline time** than Fleet's static planner on a suite with real duration variance, **with no correctness regressions**.

## Decision
**Coordinate mode is not built.** Observe mode and the static planner (ADR-0013) stay as they are. Spec §17 and ADR-0013 are not changed.

The measurements (research 13, three reps per mode, two throwaway emulators, Maestro 2.12.1):

- Static planner p50 **72.4 s**. The threshold for a 20% gain is **57.9 s**.
- Dynamic prototype, one process per flow: p50 **134.0 s**, 85% slower.
- Dynamic prototype, three flows per process: p50 **89.3 s**, 23% slower.
- Equal-count `maestro test --shard-split` p50 68.3 s. Its spread overlaps the static planner's, so the planner did not measurably beat it on this suite.
- With one device killed mid-run, the static run lost six of twelve flows. The dynamic prototype re-ran the killed flow, but three flows that should pass reported `failed` on the same device straight after the kill. That is a correctness regression.

Both conditions of the rule fail, so the rule rejects coordinate mode. The study used a synthetic 12-flow suite with designed variance, not a real customer suite. The rejection is therefore a result about this suite and this prototype. It is not a claim that real suites never have enough variance to pay off.

## Consequences
- No code, no new binary, no server work. The planner, the agent and the spec are unchanged.
- The I1, I2 and I3 answers in research 13 §4 are recorded for the file. They are not implemented.
- Marathon Cloud's rule that a fail-then-pass test is flaky and still passes the run's exit code (research 13 §9.2) is not Fleet's rule. If coordinate mode is reopened, I3 applies: a retried-then-passed flow is never plain passed.
- Follow-up work, listed here and not opened as issues, applies only if the decision is reopened: (a) a device reset after a killed Maestro process, verified to clear the `DeviceServerDiedException` state seen in research 13 §2.4; (b) a classifier that retries only infrastructure failures, never test verdicts; (c) a lease table and worker heartbeats, with an expiry, for a dynamic queue; (d) the I1 check that a run with a missing final verdict fails the job; (e) the untested constraints: `executionOrder` units, exclusive groups, shared accounts and per-device limits inside a queue.

## Reopen when
Any one of these, with measurements in the same style as research 13:

1. A real customer suite, with per-flow durations from CI history, where the static planner's p50 leaves at least 20% of the pipeline as idle device time, and a dynamic queue would recover at least 20% of pipeline time with no correctness regression.
2. A measured per-invocation overhead much smaller than the 6 to 10 s seen here, so that single-flow queue items stop paying for their JVM start. Batching changes this, and batching is the first thing to test.
3. A device reset that makes a worker recover reliably after a kill. Without it, a dynamic queue turns passing flows into failures.
4. Maestro Cloud or Marathon Cloud shows that its dynamic scheduling pays on the same suite, and Fleet can measure the same suite on its own runners.

## Sources
- Research 13 (method, raw numbers, the decision computation, documentation checked 2026-11-09): [13-coordinate-mode.md](../../research/13-coordinate-mode.md).
- Raw results and the benchmark: [13-assets](../../research/13-assets/README.md).
- ADR-0013 (static planner, the "Static only" rule), ADR-0002 (supervisor/observer split).
