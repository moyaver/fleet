// SPDX-License-Identifier: AGPL-3.0-only
//! Rate limiting for refetches triggered by live updates: at most one run per interval, and a
//! burst of requests collapses into one trailing run that sees the newest state.

/// What to do about a request to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run now.
    RunNow,
    /// Run in this many milliseconds (the caller sets a timer and calls [`Gate::fire`]).
    After(i64),
    /// A run is already scheduled and will see this request's data.
    Scheduled,
}

/// At most one run per `min_interval_ms`, with trailing-edge coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    min_interval_ms: i64,
    last_run_ms: Option<i64>,
    scheduled: bool,
}

impl Gate {
    /// A gate that lets runs through at least `min_interval_ms` apart.
    #[must_use]
    pub fn new(min_interval_ms: i64) -> Self {
        Self {
            min_interval_ms,
            last_run_ms: None,
            scheduled: false,
        }
    }

    /// Asks to run at `now_ms`. After [`Decision::RunNow`] the run counts as started.
    pub fn request(&mut self, now_ms: i64) -> Decision {
        if self.scheduled {
            return Decision::Scheduled;
        }
        let wait = self
            .last_run_ms
            .map_or(0, |last| (last + self.min_interval_ms - now_ms).max(0));
        if wait == 0 {
            self.last_run_ms = Some(now_ms);
            Decision::RunNow
        } else {
            self.scheduled = true;
            Decision::After(wait)
        }
    }

    /// The timer of a [`Decision::After`] fired at `now_ms`: the scheduled run starts.
    pub fn fire(&mut self, now_ms: i64) {
        self.scheduled = false;
        self.last_run_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_request_runs_at_once_and_a_burst_collapses_into_one_trailing_run() {
        let mut gate = Gate::new(1000);
        assert_eq!(gate.request(0), Decision::RunNow);
        assert_eq!(gate.request(100), Decision::After(900));
        assert_eq!(gate.request(200), Decision::Scheduled);
        assert_eq!(gate.request(999), Decision::Scheduled);
        gate.fire(1000);
        assert_eq!(
            gate.request(1500),
            Decision::After(500),
            "interval counts from the fire"
        );
        gate.fire(2000);
        assert_eq!(
            gate.request(5000),
            Decision::RunNow,
            "quiet for long enough"
        );
    }

    #[test]
    fn a_request_exactly_one_interval_later_runs_at_once() {
        let mut gate = Gate::new(1000);
        assert_eq!(gate.request(0), Decision::RunNow);
        assert_eq!(gate.request(1000), Decision::RunNow);
    }
}
