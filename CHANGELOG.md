# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Relicensed from PolyForm Internal Use 1.0.0 to the GNU AGPL v3 (`AGPL-3.0-only`); a commercial license is available on request (ADR-0012).
- Renamed the product to Fleet; the agent binary is `fleet-agent` (ADR-0011).

### Added

- M0 repository bootstrap: Cargo workspace skeleton, `cargo xtask` (setup, ci, lint, web), CI and CLA workflows,
  licensing and governance files, mdBook skeleton, and a Leptos hello page served by `fleet serve`.
