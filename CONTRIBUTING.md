# Contributing to Maestro Fleet

Thanks for helping. Maestro Fleet is **source-available**, not open source; read [LICENSING.md](LICENSING.md) before you start.

## 1. Sign the CLA

Every contribution needs a signed Contributor License Agreement. The [CLA workflow](.github/workflows/cla.yml) checks every pull request.

- **Individual:** read [CLA.md](CLA.md), then post this exact comment on your pull request:

  ```
  I have read the CLA Document and I hereby sign the CLA
  ```

  The workflow records your signature on the `cla-signatures` branch and re-runs the check. You sign once.
- **On company time:** your employer owns that work, so your employer signs the [Corporate CLA](CLA-CORPORATE.md) and lists you in Schedule A. This includes the owner's coworkers. Once the signed copy is in, the owner adds your GitHub login to the `cla-signatures` branch.

Every commit in a pull request must be linked to a GitHub account (commit with an email your account knows), otherwise the check can't tell who signed.

## 2. Set up

You need [rustup](https://rustup.rs). The toolchain version comes from [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
cargo xtask setup
```

This installs the pinned tools (cargo-nextest, cargo-deny, cargo-about, Trunk, mdBook) and the rustup targets.

| Command | What it does |
|---|---|
| `cargo xtask ci` | Everything CI runs: fmt, clippy, lint, deny, tests, web build |
| `cargo xtask lint` | Repository rules: SPDX headers, no JS/Node, crate dependency rules |
| `cargo xtask web [--release]` | Build the dashboard into `crates/fleet-web/dist` |
| `cargo run -p fleet-server -- serve` | Serve the dashboard and API on <http://127.0.0.1:8080> |

## 3. Rules that CI enforces

The invariants in [PLAN §3](docs/PLAN.md#3-invariants-non-negotiable) are non-negotiable. The ones you'll meet first:

- **No Node.js, npm or hand-written JavaScript/TypeScript** (I9). Everything is Rust, SQL, CSS, HTML, YAML.
- **No provider-specific code** (AWS, GCP, Azure, Cloudflare, Kubernetes) in core crates (I8).
- **Crate dependency rules** ([PLAN §4.2](docs/PLAN.md#42-dependency-rules-checked-by-cargo-xtask-lint)): for example `fleet-agent` never depends on `fleet-store` or `fleet-server`.
- **Every `.rs` file starts with** `// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0`.
- **No `unsafe`.** The workspace denies `unsafe_code`. The only planned exception is `fleet-agent::sys` (signals and process groups), which must `#![allow(unsafe_code)]` locally and document every block with a `// SAFETY:` comment.
- **Never invent Maestro output.** Parsers and tests are built from captured fixtures under `fixtures/maestro/` or quoted source. Mark anything else `// UNVERIFIED`.
- **Only permissive dependency licenses** ([`deny.toml`](deny.toml)). No GPL, AGPL, LGPL or SSPL in anything linked into a binary.

## 4. Pull requests

- One logical change per pull request.
- [Conventional Commits](https://www.conventionalcommits.org/): `feat(agent): …`, `fix(server): …`, `docs: …`, `chore(ci): …`. Scopes are crate names without the `fleet-` prefix, plus `ci`, `docs`, `xtask`.
- `cargo xtask ci` passes locally.
- Update the docs and [CHANGELOG.md](CHANGELOG.md) (`Unreleased`) when behaviour changes.
- `main` keeps a linear history: pull requests are squash- or rebase-merged.

## 5. Architecture decisions

Significant decisions are recorded as ADRs in [`docs/decisions/`](docs/decisions/README.md).

1. Copy the format of an existing ADR into `docs/decisions/NNNN-short-title.md` with the next free number.
2. Status starts as **Proposed**. It becomes **Accepted** when the owner merges it, and **Superseded** (with a link) when replaced.
3. Add it to the index in `docs/decisions/README.md`.

You need an ADR to: bump a minor or major version of a core dependency (for example Leptos 0.9, axum 0.9), break an invariant, or change the wire protocol.

## 6. Legal text

**No AI-generated legal text.** License, CLA and Code of Conduct texts are fetched verbatim from their official sources and changed only where the file says so. Don't edit them in a pull request; open an issue instead.

## 7. Conduct and security

- Everyone follows the [Code of Conduct](CODE_OF_CONDUCT.md).
- Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md). Never in a public issue.
