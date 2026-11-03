# Maestro Fleet

Maestro Fleet is self-hosted, live observability for [Maestro](https://github.com/mobile-dev-inc/maestro) mobile UI tests running in CI.
It shows what is running and on which device, how far each run has got, how the runner and emulator are coping, and why each failure happened.

> **Status:** pre-release. The repository is bootstrapped (milestone M0); there is no usable agent or server yet.
> See the [roadmap tracker](https://github.com/moyaver/maestro-fleet#roadmap) for progress.

It has three parts, all in Rust:

- **`maestro-fleet`**, the agent: wraps `maestro test` in CI without changing its exit status or output, and streams what it observes.
- **`fleet`**, the server: stores runs in PostgreSQL and artifacts in S3-compatible storage, and pushes live updates.
- **The dashboard**: a Leptos web app, embedded in the server.

Design documents live in the repository: the [implementation plan](https://github.com/moyaver/maestro-fleet/blob/main/docs/PLAN.md),
the [architecture decisions](https://github.com/moyaver/maestro-fleet/blob/main/docs/decisions/README.md) and the
[research reports](https://github.com/moyaver/maestro-fleet/blob/main/research/00-synthesis.md).
