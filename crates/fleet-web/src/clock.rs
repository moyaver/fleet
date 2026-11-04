// SPDX-License-Identifier: AGPL-3.0-only
//! Wall-clock helpers: server timestamps to unix milliseconds, and the browser's "now".

use time::OffsetDateTime;

/// `t` as unix milliseconds (saturating).
#[must_use]
pub fn unix_ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

/// The browser's current time in unix milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Date.now() is a whole number of milliseconds far below 2^63"
    )]
    let ms = js_sys::Date::now() as i64;
    ms
}
