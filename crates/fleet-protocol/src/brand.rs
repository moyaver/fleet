// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0
//! Every product name in one place (PLAN §4.1), so a rename touches only this file,
//! `docs/book.toml` and the crate directory names.

/// Human-facing product name.
pub const PRODUCT_NAME: &str = "Maestro Fleet";
/// The CI agent binary.
pub const AGENT_BIN: &str = "maestro-fleet";
/// The server binary.
pub const SERVER_BIN: &str = "fleet";
/// Prefix of every environment variable the agent and server read.
pub const ENV_PREFIX: &str = "FLEET_";
/// WebSocket subprotocol spoken between agents and the server.
pub const WS_SUBPROTOCOL: &str = "fleet.v1+json";
/// Default audience of the GitHub Actions OIDC token the agent exchanges.
pub const OIDC_AUDIENCE: &str = "maestro-fleet";
