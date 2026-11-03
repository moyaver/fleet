# ADR-0004: Dashboard technology: Leptos CSR, all Rust

**Status:** Accepted (owner decision: "full Rust") · **Date:** 2026-11-03 · **Research:** [04](../../research/04-frontend-rust-vs-ts.md)

## Context
The dashboard needs virtualized filterable tables, live time-series charts, a per-device Gantt/swimlane timeline, a screenshot and log viewer, accessible form primitives and SSO. Research 04 compared React+TS (E), server-rendered Rust + Datastar (D) and Leptos CSR (A+F). It recommended E for staffing reasons. The owner chose an all-Rust codebase with no JavaScript.

## Decision
- **Leptos `=0.8.23` CSR** (no SSR or hydration), built with **Trunk 0.22**, embedded in the server binary via `fleet-web-assets`.
- **Own component kit (`fleet-ui`)** built on native elements (`<dialog>`, Popover API, `<details>`, native inputs) and WAI-ARIA APG patterns. No third-party Leptos UI library.
- **Native Rust charts** (SVG + LTTB downsampling; canvas for large series) and a **native canvas swimlane timeline** with an accessible table alternative. No uPlot or ECharts.
- **Tailwind 4 standalone CLI** (no Node). No hand-written JS (invariant I9). The only exception is vendored axe-core inside E2E tests.
- Types come straight from `fleet-protocol`, with no codegen step.

## Consequences
- An estimated 4–10 extra engineer-weeks over a React stack, spent mostly on accessible primitives, charts and the timeline. They are scheduled as the ordered `fleet-ui` backlog in PLAN §10.2.
- Fine-grained per-entity signals and snapshot-epoch list keys are required (research 04 §3 pitfalls).
- The Leptos 0.9 upgrade gets its own ADR after 0.9.0 final.
- Contributors must know Rust. That is acceptable for a personal, source-available project.
