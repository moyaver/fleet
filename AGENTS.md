# Fleet: notes for coding agents

Fleet is live observability for mobile UI tests in CI, Maestro first (ADR-0011): a Rust agent, server and Leptos dashboard in one Cargo workspace. AGPL-3.0-only (ADR-0012).

- **Before writing or reviewing Rust**, read [docs/CONVENTIONS.md](docs/CONVENTIONS.md). New code passes the workspace lints with no bare `#[allow]`.
- **Before changing behaviour**, check the invariants in [docs/PLAN.md §3](docs/PLAN.md#3-invariants-non-negotiable); a change that would break one stops and goes to the owner.
- **Done means `cargo xtask ci` passes.** It is exactly what CI runs.
- **Maestro output comes from captured fixtures** (`fixtures/maestro/`, `cargo xtask fixtures`), never from memory.
- **Legal texts** (`LICENSE`, `CLA*.md`, `CODE_OF_CONDUCT.md`) are fetched verbatim from their official sources and edited only where the file says so.
- **Commits** are Conventional Commits, one logical change per pull request (CONTRIBUTING.md).
