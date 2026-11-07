// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end tests (PLAN §13): the real `fleet-agent` binary, wrapping a fake Maestro that
//! replays captured fixtures, against the real server on Postgres 18. The tests live in
//! `tests/it`; browser tests (fantoccini + chromedriver, `tests/it/browser.rs`) need `cargo xtask setup`
//! and a built dashboard (`cargo xtask web`).
