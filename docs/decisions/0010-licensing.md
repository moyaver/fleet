# ADR-0010: Source-available licensing, CLA, donations

**Status:** Superseded by [ADR-0012](0012-agpl-license.md) · **Date:** 2026-11-03 · **Detail:** [PLAN §2](../PLAN.md#2-licensing-contributions-and-donations)

## Context
The owner wants the code public on a personal GitHub account and usable by their employer. Copying and redistribution should be forbidden, contributions and donations welcome. Every OSI open source license must allow redistribution, so the project is **source-available**, not open source.

## Decision
- **License:** PolyForm Internal Use License 1.0.0, verbatim, with a Required Notice naming the owner. It allows internal business use and internal changes; it forbids distribution.
- **Cargo:** `license-file` + `publish = false`. SPDX header `AGPL-3.0-only`.
- **Contributions** require an Individual CLA, or a Corporate CLA for work done on company time. Both are adapted from the ASF CLAs and grant a broad license including relicensing, not an assignment. Enforced by the CLA Assistant Lite action.
- **Donations** via GitHub Sponsors (`FUNDING.yml`). Donations grant no license rights.
- **Dependencies:** permissive only in linked code (cargo-deny); notices shipped via cargo-about. Garage (AGPL) stays a separate service.
- **Precondition:** written employer confirmation that the project is the owner's IP (PLAN §2.5).

## Consequences
The project can't be called "open source" or use OSI-only programs. Some contributors and company legal teams may hesitate; `LICENSING.md` explains the terms plainly. Relicensing later (e.g. to an OSI license) stays possible thanks to the CLA.
