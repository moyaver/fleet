# ADR-0012: Open source under AGPL-3.0-only, CLA kept for a commercial license

**Status:** Accepted (owner decision) · **Date:** 2026-11-04 · **Supersedes:** [ADR-0010](0010-licensing.md)

## Context
ADR-0010 chose a source-available license (PolyForm Internal Use) to forbid redistribution. The owner now wants Fleet to be real open source that accepts contributions, while stopping anyone from taking the project and making it their own closed product.

## Decision
- **License:** GNU AGPL v3, `AGPL-3.0-only`, text fetched verbatim from <https://www.gnu.org/licenses/agpl-3.0.txt> into `LICENSE`. Every source file carries `SPDX-License-Identifier: AGPL-3.0-only`; Cargo uses `license = "AGPL-3.0-only"`.
  - Use and modification are free for everyone, including internal company use.
  - Distribution, and network use of a modified version, require offering the source under the AGPL. No closed forks, no closed SaaS variants.
  - `-only` rather than `-or-later`: future license versions are adopted deliberately, through the CLA, not automatically.
- **Contributions:** the existing Individual and Corporate CLAs stay. They grant the owner the right to sublicense, so the owner can **dual-license**: AGPL for everyone, a commercial license on request.
- **Dependencies:** still permissive only (`deny.toml`), because copyleft dependencies would block the commercial license.
- **Name:** forks may not present themselves as "Fleet" or as endorsed by the owner (stated in `LICENSING.md`). This is a plain statement, not a registered trademark.
- **Donations** via GitHub Sponsors, unchanged.

## Consequences
Fleet can be called open source. Some organizations ban AGPL software internally; the commercial license covers them. The owner is the only copyright holder so far, so relicensing earlier commits needs nobody else's consent. ADR-0010 and PLAN §2 are kept as history.
