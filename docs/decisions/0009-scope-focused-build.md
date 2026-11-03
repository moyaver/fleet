# ADR-0009: Scope: focused build

**Status:** Proposed · **Date:** 2026-11-03 · **Research:** [07 §6](../../research/07-prior-art-and-build-vs-buy.md)

## Decision
**Build** what nobody offers for company-run CI emulators:
- the agent;
- live ingestion and state;
- the Maestro-aware data model with provenance;
- device and host health correlated with the flow timeline;
- a lean evidence viewer.

**Build thin:**
- history and trends (SQL over Fleet tables);
- flakiness: *confirmed* = pass and fail on the same commit under the same condition fingerprint; *suspected* = flip rate ≥ 0.2 over the last 20 default-branch runs; infrastructure failures and failures during host anomalies excluded;
- failure fingerprinting: exception type + step identity + normalized message, versioned, with user override rules.

**Don't build:** ML triage, a time-series DB, quality gates, test management, a report generator.

**Interop:** OTel `cicd.*`/`test.*` names; optional OTLP export from the agent; optional Allure and ReportPortal exporters.

## Fallback
If capacity is short: agent + live view, with final results pushed to ReportPortal or Allure 3. Evaluate this in Phase 0 (1–2 days) on the real fixtures.
