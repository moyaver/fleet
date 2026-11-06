# ADR-0002: Agent supervisor/observer process split, fail-open

**Status:** Accepted (shipped in M2, 2026-11-06) · **Date:** 2026-11-03 · **Research:** [02 §1, §15](../../research/02-rust-agent.md)

## Context
Spec invariant: Fleet must never change Maestro's exit code or CI output. A single async process that does networking, parsing and adb calls can panic, hang or OOM.

## Decision
`maestro-fleet run` is the **supervisor** (P1). It uses std only: no tokio, no sockets, no parsing. It:
- spawns Maestro in its own process group, from the main thread;
- tees stdout and stderr byte-for-byte to the CI log;
- forwards signals;
- exits with Maestro's code, or re-raises the same signal.

P1 spawns an **observer** (P2, tokio) that does all telemetry. P1 feeds P2 a copy of the output through a non-blocking pipe; if P2 falls behind, the copy is dropped.

**Fail open:** bad config, credentials or server means Maestro still runs, unmonitored, with a warning. Only `check` and `selftest` fail hard. The agent writes nothing to stdout.

## Consequences
- A telemetry crash cannot alter the CI result (verified in the PoC).
- On a GitHub cancel (SIGINT, 7.5 s, SIGTERM, 2.5 s, kill), the observer sends `session.interrupted` at the first signal. Cleanup is best-effort.
- Slightly more complex IPC (data pipe + control pipe).
