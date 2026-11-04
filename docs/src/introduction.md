# Fleet

Fleet is self-hosted, live observability for mobile UI tests running in CI, starting with [Maestro](https://github.com/mobile-dev-inc/maestro).
It is not affiliated with mobile.dev, the makers of Maestro.
It shows what is running and on which device, how far each run has got, how the runner and emulator are coping, and why each failure happened.

> **Status:** pre-release. The agent (M2) and the server core (M3) work end to end: agents publish to a self-hosted server over WebSocket or HTTPS, and the REST API and live socket serve the state. The dashboard is next (M4).
> See the [roadmap tracker](https://github.com/moyaver/fleet#roadmap) for progress.

It has three parts, all in Rust:

- **`fleet-agent`**, the agent: wraps `maestro test` in CI without changing its exit status or output, and streams what it observes.
- **`fleet`**, the server: stores runs in PostgreSQL and artifacts in S3-compatible storage, and pushes live updates.
- **The dashboard**: a Leptos web app, embedded in the server.

Design documents live in the repository: the [implementation plan](https://github.com/moyaver/fleet/blob/main/docs/PLAN.md),
the [architecture decisions](https://github.com/moyaver/fleet/blob/main/docs/decisions/README.md) and the
[research reports](https://github.com/moyaver/fleet/blob/main/research/00-synthesis.md).
