# Fleet

[![CI](https://github.com/moyaver/fleet/actions/workflows/ci.yml/badge.svg)](https://github.com/moyaver/fleet/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE)

Self-hosted, live observability for mobile UI tests running in CI, starting with [Maestro](https://github.com/mobile-dev-inc/maestro).
It shows what is running and on which device, how far each run has got, how the runner and emulator are coping, and why each failure happened.

> [!NOTE]
> **Status: M0 (bootstrap).** The workspace, tooling, CI and licensing are in place; there's no product logic yet.
> Next up is **M1**: capturing real Maestro output on emulators. Progress is tracked [below](#roadmap).

Everything is **Rust**: the agent, the server and the dashboard. There's no Node.js, npm or hand-written JavaScript anywhere.

> Fleet supports Maestro first; the core is runner-agnostic so other test runners can follow ([ADR-0011](docs/decisions/0011-name-and-runner-scope.md)).
> It is an independent project, not affiliated with mobile.dev, the makers of Maestro.

---

## Roadmap

Each milestone has exit criteria in [PLAN §12](docs/PLAN.md#12-milestones). A milestone starts only when the previous one's criteria pass or the owner waives them.
Issues are grouped by [GitHub milestone](https://github.com/moyaver/fleet/milestones).

| | Milestone | Scope | Exit criteria | Estimate |
|---|---|---|---|---|
| ✅ | [**M0** Bootstrap](https://github.com/moyaver/fleet/milestone/1) | Repo, licensing, workspace skeleton, xtask, CI, hello page | `cargo xtask ci` green locally and in CI; `fleet serve` shows the hello page; license files verbatim | 1–2 days |
| ⏭️ | [**M1** Phase 0: Maestro proof](https://github.com/moyaver/fleet/milestone/2) | Capture real Maestro 2.12.x runs on emulators as fixtures; answer the 17 open questions | Every progress state classified observable / inferred / unavailable with fixture evidence; device-correlation rule confirmed | 1–2 weeks |
| ⬜ | [**M2** Agent](https://github.com/moyaver/fleet/milestone/3) | Parsers, supervisor/observer, metrics, adb, spool, transport, signed releases | Invariants I1, I2, I5, I6, I10 proven by tests; < 1% CPU overhead; a real CI run streams unchanged | 3–5 weeks |
| ⬜ | [**M3** Server core](https://github.com/moyaver/fleet/milestone/4) | Migrations, ingest, acks/resume, live hub, REST, OpenAPI, agent auth, Compose | Concurrent agents don't corrupt state; restarts lose no committed events; acceptance criteria 3, 10, 11, 15 | 4–6 weeks |
| ⬜ | [**M4** Dashboard core](https://github.com/moyaver/fleet/milestone/5) | `fleet-ui` kit, screens A–D, browser OIDC login, E2E | A developer can follow a full run without CI logs; acceptance criteria 2, 4, 5, 6, 14 | 4–6 weeks |
| ⬜ | [**M5** Evidence and history](https://github.com/moyaver/fleet/milestone/6) | Artifacts, usercontent origin, screenshot and log viewers, fingerprints, flaky v1, screens E–F | A failed run can be diagnosed after the runner is gone; acceptance criteria 7, 8, 9 | 3–4 weeks |
| ⬜ | [**M6** Hardening and v1.0.0](https://github.com/moyaver/fleet/milestone/7) | RBAC, admin screen, security review, load tests, runbooks, guides, release | All 15 acceptance criteria pass; IT can deploy from the docs alone | 3–4 weeks |
| ⬜ | [**M7+** Optional](https://github.com/moyaver/fleet/milestone/8) | Notifications, OTLP, GitLab/Buildkite, fork-PR relay, iOS, Leptos 0.9 | — | — |

✅ done · 🚧 in progress · ⏭️ next · ⬜ not started

### M0 checklist

- [x] Repository on the owner's personal account, private until the owner makes it public
- [x] Spec, plan, ADRs and research imported unchanged
- [x] `LICENSE` (GNU AGPL v3, verbatim; [ADR-0012](docs/decisions/0012-agpl-license.md)), `LICENSING.md`, `CLA.md`, `CLA-CORPORATE.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1, verbatim), `SECURITY.md`, `FUNDING.yml`
- [x] Workspace skeleton: every crate compiles, SPDX headers, `publish = false`, `license`, workspace lints
- [x] `cargo xtask setup | ci | lint | web`
- [x] `deny.toml` and `about.toml`
- [x] `ci.yml` and `cla.yml`
- [x] Leptos hello page built by Trunk, embedded and served by `fleet serve` with the CSP
- [x] mdBook skeleton under `docs/`
- [x] GitHub settings: Discussions, merge options, topics, labels, milestones, `main` ruleset, private vulnerability reporting, secret scanning

### Owner actions

Things only the owner can do. Each one has an issue labelled [`owner-action`](https://github.com/moyaver/fleet/labels/owner-action).

- [x] **Before product code:** written confirmation from the employer that this is personal IP ([#1](https://github.com/moyaver/fleet/issues/1), [PLAN §2.5](docs/PLAN.md#25-precondition-employer-ip-check-owner-action-before-m0-code))
- [x] Decide the final product name: **Fleet** ([#5](https://github.com/moyaver/fleet/issues/5), [ADR-0011](docs/decisions/0011-name-and-runner-scope.md))
- [ ] Ask a lawyer to review the AGPL + commercial-license model, the CLA adaptations and the employer letter ([#6](https://github.com/moyaver/fleet/issues/6))
- [x] Add a public contact email for Code of Conduct reports and Corporate CLA delivery ([#2](https://github.com/moyaver/fleet/issues/2))
- [ ] Set up a GitHub Sponsors profile ([#3](https://github.com/moyaver/fleet/issues/3))
- [x] When public: branch protection on `main`, private vulnerability reporting, secret scanning ([#4](https://github.com/moyaver/fleet/issues/4))

---

## How it works

```
 CI runner                                        Fleet server                     Browser
┌──────────────────────────────────────┐        ┌───────────────────────┐        ┌────────────┐
│ fleet-agent run -- maestro test …    │        │ fleet serve           │        │ dashboard  │
│  ├─ supervisor: runs Maestro,        │  WSS   │  ├─ ingest (journal + │  WSS   │ (Leptos,   │
│  │  passes output and exit through   │ ─────► │  │  telemetry)        │ ─────► │  embedded) │
│  └─ observer: maestro.log, stdout,   │ OIDC   │  ├─ live hub          │        └────────────┘
│     bundle folders, JUnit, metrics,  │        │  ├─ REST + OpenAPI    │
│     adb devices                      │        │  └─ background jobs   │
└──────────────────────────────────────┘        └───┬───────────────┬───┘
                                                    │               │
                                              PostgreSQL 18   S3-compatible
                                                              storage (Garage)
```

- **The agent never changes Maestro's result.** It fails open: if Fleet is down or misconfigured, the test run carries on unchanged.
- **Nothing is falsely green.** Every displayed result carries a provenance (observed, provisional, final, derived, inferred, missing, stale), and only direct observation can claim `passed`.
- **Secrets are redacted on the runner** before anything leaves it.

The full list of invariants is in [PLAN §3](docs/PLAN.md#3-invariants-non-negotiable).

## Repository layout

```
crates/
  fleet-protocol/    wire types, REST DTOs, JSON Schema; no async runtime, wasm-safe
  fleet-core/        pure domain: state machines, claim lattice, reconciliation, redaction, policy
  fleet-maestro/     Maestro version profiles and parsers (fixture-driven)
  fleet-agent/       `fleet-agent` CI agent
  fleet-store/       PostgreSQL repositories and migrations (sqlx)
  fleet-blob/        S3-compatible and filesystem blob storage
  fleet-server/      `fleet` server: REST, WebSockets, auth, jobs, embedded dashboard
  fleet-ui/          Leptos component kit
  fleet-web/         Leptos CSR dashboard, built by Trunk
  fleet-web-assets/  embeds the built dashboard into the server
  fleet-loadgen/     protocol load generator
  fleet-e2e/         browser and full-stack tests
xtask/               cargo xtask: setup, ci, lint, web
docs/                mdBook, implementation plan, spec, ADRs
research/            research reports 00–07 and Maestro CLI fixtures
```

## Development

You need [rustup](https://rustup.rs); the toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
cargo xtask setup
```

```bash
cargo xtask ci
```

```bash
cargo xtask web && cargo run -p fleet-server -- serve
```

Then open <http://127.0.0.1:8080>. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules CI enforces.

## Documentation

| Document | What it is |
|---|---|
| [Implementation plan](docs/PLAN.md) | The single source of truth: decisions, invariants, stack, milestones |
| [Research synthesis](research/00-synthesis.md) | Conclusions of research reports 01–07 on one page |
| [Spec v1.0](docs/spec-v1.0.md) | The original product and architecture specification |
| [Architecture decisions](docs/decisions/README.md) | ADRs 0001–0011 |
| [Book](docs/src/SUMMARY.md) | User, runner and operator guides (`mdbook serve docs`), filled in from M2 |

## License

Fleet is **open source** under the [GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).

- Anyone may use, study, modify and share it, including companies running it internally, for free.
- If you distribute Fleet, or let people use a modified Fleet over a network, you must offer them the complete source of your version under the same license. Nobody can take it closed.
- Forks must not call themselves Fleet or suggest they are the official project.

Need different terms, for example to embed Fleet in a proprietary product? A commercial license is available: email <mhammadjaber00@outlook.com>. Read the plain-language [Licensing FAQ](LICENSING.md).

## Contributing and sponsoring

Contributions are welcome under a [Contributor License Agreement](CLA.md); see [CONTRIBUTING.md](CONTRIBUTING.md).
If Fleet helps you, you can [sponsor its development](https://github.com/sponsors/moyaver). Sponsorship buys no license rights.

Please follow the [Code of Conduct](CODE_OF_CONDUCT.md), and report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
