# ADR-0016: Fleet is a hub for mobile e2e tests

**Status:** Accepted (owner decision, 2026-11-09) · **Date:** 2026-11-09 · **Supersedes:** [ADR-0009](0009-scope-focused-build.md) (scope) · **Amends:** [spec v1.0](../spec-v1.0.md) §1 (vision) and §17 (non-goals) · **Plan:** [research 16](../../research/16-hub-plan.md)

## Context

The founder's sentence for the product: *"Somewhere I can handle my e2e tests easily: run them on CI, watch them, organize them, pick what to run, and when."*

Fleet, as built through M5 and alpha.4, watches Maestro runs. It does not yet hold the run history a team needs to act on it: one record per app build across runners, a release page to share, a place to organize flows, or a way to choose what runs. ADR-0009 kept Fleet narrow on purpose. The founder's sentence needs more than that.

Before this decision, an earlier direction (a CI-step product with no server) was looked at and withdrawn before it merged. This ADR does not return to it. The server is the product's foundation.

Buyer conversations for this direction were simulated. They are reasoning about who might pay and why, not data. No price, demand or revenue figure in this ADR or in research 16 rests on them. Real validation is tracked in issue #205, and it starts with ten or more conversations with real teams.

## Decision

Fleet is a hub for mobile end-to-end tests. Teams run their tests in their own CI, and Fleet shows the results, organizes them, and helps choose what runs.

1. **The core entity is the build key and the release record.** The key is `(app_id, platform, version, build)` plus the commit SHA. Every run of one build attaches to one record. Infrastructure failures are kept apart from test failures on that record. The key comes from the device first, then from `--app`, then from a user-declared `FLEET_BUILD` (research 16 §3.1).
2. **Fleet stays one AGPL-3.0-only binary.** A hosted tier is a configuration of the same binary. Everything stays self-hostable. Hosted decisions are in ADR-0017, written with its wave.
3. **Fleet never runs tests or provides devices.** The team's CI runs them. Fleet triggers the team's CI only for a guarded rerun of named flows, and reads a selection (ADR-0019, written with its wave).
4. **No Fleet-owned schedules (owner decision, 2026-11-09).** The team's CI cron decides when a run happens. Fleet decides what runs through `--selection`. Fleet does not store or dispatch schedules. This removes the opt-in schedule that research 16 first proposed.
5. **Fleet still never changes a test's outcome.** The invariants in [PLAN §3](../PLAN.md#3-invariants-non-negotiable) stand. The hub adds I12 (no accuracy figure without its labelled set), I13 (selection and quarantine never hide a failure) and I14 (tenants never see each other). I2 is widened: a Fleet outage never prevents or fails a run.

**Not planned:** Fleet running tests or providing devices; AI root cause; test selection by code change; per-seat, committer or device-hour pricing; pixel redaction; guessing at leftover test state; automatic retries; Fleet-owned schedules; SOC 2 and EU residency within this horizon.

## Consequences

- **ADR-0009 is superseded for scope.** Its "don't build" list (test management, quality gates) no longer describes the product's direction for quarantine, selection and reruns. Quarantine (ADR-0018) and guarded reruns and selection (ADR-0019) decide those when their waves start. Quality gates stay out: Maestro's exit code remains the gate.
- **Spec v1.0 §1 and §17 are amended.** The spec text is kept verbatim behind a header that lists the superseded sections.
- **The invariants grow.** PLAN §3 gains I12, I13 and I14. Each has a named test in its wave.
- **ADRs 0017 to 0020 are planned, not written.** They come with their waves. Accepted decisions 0001 to 0008 and 0011 to 0015 stay as they are; 0010 stays superseded by 0012. 0013 (the planner) stays accepted and is not marketed. ADR-0015 (coordinate mode) stays rejected.
- **The README states the status plainly.** Self-hosted alpha today; no hosted tier yet; Android today; iOS next. A feature appears as available only when its wave is merged on `main`.
- **Honest limits stay on record.** One maintainer is the largest risk. SOC 2 and EU residency are out of this horizon. Hosted sign-in will require a GitHub account (ADR-0017).
