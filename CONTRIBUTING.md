# Contributing to Fleet

Thanks for helping. Fleet is open source under the [GNU AGPL v3](LICENSE); read [LICENSING.md](LICENSING.md) before you start.

## 1. Sign the CLA

Every contribution needs a signed [Contributor License Agreement](CLA.md). The [CLA workflow](.github/workflows/cla.yml) checks every pull request.

- **Individual:** read [CLA.md](CLA.md), then post this exact comment on your pull request:

  ```
  I have read the CLA Document and I hereby sign the CLA
  ```

  The workflow records your signature on the `cla-signatures` branch and re-runs the check. You sign once.
- **On company time:** your employer signs the [Corporate CLA](CLA-CORPORATE.md) and lists you in its Schedule A. Email the signed copy to <mhammadjaber00@outlook.com>. The owner then adds your GitHub login to the `cla-signatures` branch.

Every commit in a pull request must link to a GitHub account. Commit with an email your account knows. Otherwise the check cannot tell who signed.

## 2. Set up

You need [rustup](https://rustup.rs). The toolchain version comes from [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
cargo xtask setup
```

It installs the toolchain, components and targets from `rust-toolchain.toml`. It then installs the latest cargo-nextest, cargo-deny, cargo-about, sqlx-cli, Trunk and mdBook. Tailwind and chromedriver go to `target/tools/`.

| Command | What it does |
|---|---|
| `cargo xtask ci` | Everything CI runs: schema, fmt, clippy, lint, deny, tests, web build, docs |
| `cargo xtask lint` | Repository rules: SPDX headers, no JS/Node, crate dependency rules |
| `cargo xtask web [--release]` | Build the dashboard into `crates/fleet-web/dist` |
| `cargo run -p fleet-server -- serve` | Serve the dashboard and API on <http://127.0.0.1:8080> |

The browser tests (`fleet-e2e`) never leave Chrome running. Each test's chromedriver leads its own process group, and Chrome joins it. The test's `Browser` ends the session and that group when it drops, also when the test panics. Each Chrome runs on a profile under `$TMPDIR/fleet-e2e-chrome/` and removes it when the browser ends. A test process killed outright (no drop) is cleaned up by the next fleet-e2e run. That run ends the Chrome processes whose profile belongs to a test process that is no longer alive, and deletes those profiles. It never touches a Chrome whose profile is outside that directory.

Two checkouts can share one Postgres server, but not run their `#[sqlx::test]` tests at the same time. sqlx 0.9 names each such test's database `_sqlx_test_<hash>`. The hash comes from the test's module path and name, with no per-process or per-checkout part. Two runs of the same test therefore both create that database. The second fails with `duplicate key … _sqlx_test_<hash> already exists`. Give each checkout its own server (a container on its own port), or run one checkout's database tests at a time. The other database tests name their databases `fleet_test_<uuid>` (`fleet_store::testing::fresh_database`), so they do not collide.

## 3. Rules

The invariants in [PLAN §3](docs/PLAN.md#3-invariants-non-negotiable) are non-negotiable. CI checks most of the rules below. Review checks the rest:

- **No Node.js, npm or hand-written JavaScript/TypeScript** (I9). Everything is Rust, SQL, CSS, HTML, YAML.
- **No provider-specific code** (AWS, GCP, Azure, Cloudflare, Kubernetes) in core crates (I8).
- **Crate dependency rules** ([PLAN §4.2](docs/PLAN.md#42-dependency-rules-checked-by-cargo-xtask-lint)): for example `fleet-agent` never depends on `fleet-store` or `fleet-server`.
- **Every `.rs` file starts with** `// SPDX-License-Identifier: AGPL-3.0-only`.
- **Rust conventions** are in [docs/CONVENTIONS.md](docs/CONVENTIONS.md): package layout, errors, async, logging, testing, docs and lint exceptions. Most of it is enforced by `cargo xtask ci`.
- **No `unsafe`.** The workspace denies `unsafe_code`; only `crates/fleet-agent/src/sys.rs` may opt out (the file does not exist yet), with `#![expect(unsafe_code, reason = "…")]` and a `// SAFETY:` comment on every block. Safe APIs come first.
- **Never invent Maestro output.** Parsers and tests are built from captured fixtures under `fixtures/maestro/` or quoted source. Mark anything else `// UNVERIFIED`.
- **Only permissive dependency licenses** ([`deny.toml`](deny.toml)). No GPL, AGPL, LGPL or SSPL in anything linked into a binary.

## 4. Pull requests

- One logical change per pull request.
- [Conventional Commits](https://www.conventionalcommits.org/): `feat(agent): …`, `fix(server): …`, `docs: …`, `chore(ci): …`. Scopes are crate names without the `fleet-` prefix, plus `ci`, `docs`, `xtask`. CI does not check commit messages or PR titles. Reviewers do.
- `cargo xtask ci` passes locally.
- Update the docs and [CHANGELOG.md](CHANGELOG.md) (`Unreleased`) when behaviour changes.
- `main` keeps a linear history: pull requests are squash- or rebase-merged.

## 5. Architecture decisions

Significant decisions are recorded as ADRs in [`docs/decisions/`](docs/decisions/README.md).

1. Copy the format of an existing ADR into `docs/decisions/NNNN-short-title.md` with the next free number.
2. Status starts as **Proposed**. It becomes **Accepted** when the owner merges it, and **Superseded** (with a link) when replaced.
3. Add it to the index in `docs/decisions/README.md`.

You need an ADR to break an invariant, change the wire protocol, or swap a core framework, such as Leptos. Version bumps don't need one: dependencies track their latest stable releases.

## 6. Legal text

**No AI-generated legal text.** License, CLA and Code of Conduct texts are fetched verbatim from their official sources and changed only where the file says so. Don't edit them in a pull request; open an issue instead.

## 7. Conduct and security

- Everyone follows the [Code of Conduct](CODE_OF_CONDUCT.md).
- Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md). Never in a public issue.
