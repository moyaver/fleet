// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-agent`: wraps `maestro test` in CI. A supervisor process passes Maestro's output and
//! exit status through untouched (invariant I1) while an observer process watches the run and
//! streams it to the Fleet server. See PLAN §6 and research 02.

mod adb;
mod ci;
pub mod cli;
mod commands;
mod device;
pub mod inspect;
mod ipc;
pub mod journal;
mod metrics;
mod net;
mod observer;
mod outbox;
mod prepare;
mod session;
mod supervisor;
mod transport;
mod upload;
mod watch;
