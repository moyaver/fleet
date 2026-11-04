// SPDX-License-Identifier: AGPL-3.0-only
//! The reconnect schedule: exponential from 0.5 s to a 15 s ceiling, with "equal jitter" (each
//! delay is between half its ceiling and all of it) so a restarted server isn't hit by every
//! open dashboard at the same instant.

/// The first delay's ceiling.
const BASE_MS: u64 = 500;
/// No delay ceiling goes beyond this.
const CAP_MS: u64 = 15_000;

/// The delay before reconnect number `attempt` (from 0). `jitter` is a random number in 0 to 1.
#[must_use]
pub fn delay_ms(attempt: u32, jitter: f64) -> u64 {
    let ceiling = BASE_MS
        .checked_shl(attempt)
        .filter(|c| c >> attempt == BASE_MS)
        .map_or(CAP_MS, |c| c.min(CAP_MS));
    let half = ceiling / 2;
    let jitter = jitter.clamp(0.0, 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "the product is at most CAP_MS / 2, far inside u64 and f64's exact range"
    )]
    let extra = (half as f64 * jitter).round() as u64;
    half + extra
}

/// Counts failed attempts since the last good connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// The delay before the next attempt, which counts it.
    pub fn next_delay_ms(&mut self, jitter: f64) -> u64 {
        let delay = delay_ms(self.attempt, jitter);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// A connection worked: start over.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap() {
        let ceilings: Vec<u64> = (0..7).map(|n| delay_ms(n, 1.0)).collect();
        assert_eq!(ceilings, [500, 1000, 2000, 4000, 8000, 15_000, 15_000]);
    }

    #[test]
    fn jitter_spreads_a_delay_over_its_upper_half() {
        assert_eq!(delay_ms(3, 0.0), 2000);
        assert_eq!(delay_ms(3, 0.5), 3000);
        assert_eq!(delay_ms(3, 1.0), 4000);
        assert_eq!(delay_ms(3, 7.0), 4000, "out-of-range jitter is clamped");
        assert_eq!(delay_ms(3, -1.0), 2000);
    }

    #[test]
    fn a_huge_attempt_count_does_not_overflow() {
        assert_eq!(delay_ms(u32::MAX, 1.0), 15_000);
    }

    #[test]
    fn a_good_connection_resets_the_schedule() {
        let mut b = Backoff::default();
        assert_eq!(b.next_delay_ms(1.0), 500);
        assert_eq!(b.next_delay_ms(1.0), 1000);
        assert_eq!(b.next_delay_ms(1.0), 2000);
        b.reset();
        assert_eq!(b.next_delay_ms(1.0), 500);
    }
}
