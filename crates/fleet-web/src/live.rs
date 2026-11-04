// SPDX-License-Identifier: AGPL-3.0-only
//! The browser's live data (PLAN §5.4 pitfalls, §10.1).
//!
//! - [`backoff`]: the reconnect schedule.
//! - [`queue`]: frames wait in a queue that is applied once per animation frame.
//! - [`versions`]: spots a skipped version, which the socket answers with a new snapshot.
//! - [`keys`]: which `(epoch, id)` keys a keyed list holds, so a re-snapshot rebuilds rows.
//!
//! The pure parts are tested natively. [`socket`] is the browser I/O and [`model`] the reactive
//! adapter over `fleet_ui::live_store::LiveStore`, both thin layers over them.

pub mod backoff;
pub mod keys;
pub mod model;
pub mod queue;
pub mod socket;
pub mod versions;
