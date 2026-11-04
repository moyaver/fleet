# ADR-0011: Name "Fleet"; generic core, Maestro first

**Status:** Accepted (owner decision) · **Date:** 2026-11-04 · **Resolves:** open decision O1 in [PLAN §1](../PLAN.md#1-decisions-already-made)

## Context
The working name "Maestro Fleet" starts with mobile.dev's product name and could look official. Most of the design doesn't depend on Maestro either: the supervisor, metrics, adb tracking, CI adapters, redaction, wire protocol, provenance model, server and dashboard would serve any mobile UI test runner. Only knowing *what a runner prints and writes, and when* is runner-specific.

## Decision
- **Name:** the product is **Fleet**.

  | Was | Now |
  |---|---|
  | Maestro Fleet | Fleet |
  | `moyaver/maestro-fleet` | `moyaver/fleet` |
  | agent binary `maestro-fleet` | `fleet-agent` |
  | OIDC audience default `maestro-fleet` | `fleet` |
  | server binary `fleet`, env prefix `FLEET_`, WS subprotocol `fleet.v1+json` | unchanged |

  The constants live in `fleet-protocol::brand` (PLAN §4.1). The plan, spec v1.0 and research reports keep the old names as historical text; read them through this table.
- **Scope:** v1 supports **Maestro only**, as planned. The core stays runner-agnostic:
  - Maestro knowledge lives in `fleet-maestro` and in the agent's Maestro evidence lanes, nowhere else.
  - `fleet-protocol`, `fleet-core`, `fleet-store`, `fleet-server`, `fleet-ui` and `fleet-web` get no Maestro-specific types or names. The event families (run, plan, device, suite, shard, flow, step, artifact) are already generic.
  - Another runner (for example Espresso, XCUITest, Appium) means a new crate beside `fleet-maestro`, built from captured fixtures first, and its own ADR.

## Consequences
"Fleet" is a common word in developer tools (for example Rancher Fleet and FleetDM), so search results and package names may collide. The README and docs say Fleet is not affiliated with mobile.dev. Renaming again later still only touches `fleet-protocol::brand`, `docs/book.toml` and the repository name.
