# Fleet

Fleet is the home for your mobile end-to-end tests. Run them in your own CI, watch them live, and keep every failure's evidence and history in one place. Next, it groups results by app build and lets you choose what runs.

> **Status:** Self-hosted alpha today; no hosted tier yet. Android today; iOS next.

Fleet works with [Maestro](https://github.com/mobile-dev-inc/maestro) today. It is not affiliated with mobile.dev, the makers of Maestro.
Today you can watch runs live, follow runners and devices, read failure evidence, and browse the flow catalog.
The one-record-per-build view, quarantine, Fleet-triggered reruns, test selection and schedules are planned, not built. See the [roadmap](https://github.com/moyaver/fleet/blob/main/docs/ROADMAP.md).
Fleet observes runs. It does not change a test's outcome, and a Fleet outage never fails a run.

It has three parts, all in Rust:

- **`fleet-agent`**, the agent: wraps `maestro test` in CI without changing its exit status or output, and streams what it observes.
- **`fleet`**, the server: stores runs in PostgreSQL and artifacts in S3-compatible storage or on the server's disk, and pushes live updates.
- **The dashboard**: a Leptos web app, embedded in the server.

Design documents live in the repository: the [implementation plan](https://github.com/moyaver/fleet/blob/main/docs/PLAN.md),
the [architecture decisions](https://github.com/moyaver/fleet/blob/main/docs/decisions/README.md) and the
[research reports](https://github.com/moyaver/fleet/blob/main/research/00-synthesis.md).
