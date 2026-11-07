// SPDX-License-Identifier: AGPL-3.0-only
//! Data fetched over REST that live updates ask to refresh.
//!
//! [`remote`] fetches once at once and again whenever [`Remote::request`] is called, at most once
//! per interval (a burst of live frames becomes one trailing fetch that sees the newest state).
//! A slow earlier answer never overwrites a newer one, and a failed refetch keeps the last good
//! data on screen next to its error. A `429` is not an error: the fetch waits for the server's
//! `Retry-After` and asks again, and the data on screen stays.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gloo_timers::future::TimeoutFuture;
use leptos::{prelude::*, task::spawn_local};

use crate::{
    api::ApiError,
    clock::now_ms,
    throttle::{Decision, Gate},
};

/// The latest data, the latest error, and the way to ask for fresher data.
pub struct Remote<T: Send + Sync + 'static> {
    /// The last data received; stays when a later fetch fails.
    pub data: RwSignal<Option<T>>,
    /// Why the last fetch failed; cleared by the next success.
    pub error: RwSignal<Option<ApiError>>,
    request: Rc<dyn Fn()>,
}

impl<T: Send + Sync + 'static> Clone for Remote<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data,
            error: self.error,
            request: Rc::clone(&self.request),
        }
    }
}

impl<T: Send + Sync + 'static> std::fmt::Debug for Remote<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").finish_non_exhaustive()
    }
}

impl<T: Send + Sync + 'static> Remote<T> {
    /// Asks for fresher data, subject to the interval.
    pub fn request(&self) {
        (self.request)();
    }
}

/// A `Retry-After` in milliseconds, for the timer that waits it out. `u32` is what the timer takes.
fn retry_after_ms(secs: u64) -> u32 {
    u32::try_from(secs.saturating_mul(1000)).unwrap_or(u32::MAX)
}

/// Fetches with `fetch` now, and again on [`Remote::request`], at most once per
/// `min_interval_ms`. Stops updating when the current reactive owner is cleaned up.
pub fn remote<T, Fut>(min_interval_ms: i64, fetch: impl Fn() -> Fut + 'static) -> Remote<T>
where
    T: Send + Sync + 'static,
    Fut: Future<Output = Result<T, ApiError>> + 'static,
{
    let data = RwSignal::new(None);
    let error = RwSignal::new(None);
    let fetch = Rc::new(fetch);
    let latest = Rc::new(Cell::new(0_u64));
    let gate = Rc::new(RefCell::new(Gate::new(min_interval_ms)));
    let alive = Arc::new(AtomicBool::new(true));
    {
        let alive = Arc::clone(&alive);
        on_cleanup(move || alive.store(false, Ordering::Relaxed));
    }
    let run = {
        let alive = Arc::clone(&alive);
        Rc::new(move || {
            let (fetch, latest, alive) =
                (Rc::clone(&fetch), Rc::clone(&latest), Arc::clone(&alive));
            spawn_local(async move {
                let mine = latest.get() + 1;
                latest.set(mine);
                let result = loop {
                    match fetch().await {
                        Err(ApiError::RateLimited { retry_after_secs }) => {
                            TimeoutFuture::new(retry_after_ms(retry_after_secs)).await;
                            // Superseded by a newer fetch, or the page is gone: stop waiting.
                            if latest.get() != mine || !alive.load(Ordering::Relaxed) {
                                return;
                            }
                        }
                        other => break other,
                    }
                };
                // Superseded by a newer fetch, or the page is gone.
                if latest.get() != mine || !alive.load(Ordering::Relaxed) {
                    return;
                }
                match result {
                    Ok(value) => {
                        data.set(Some(value));
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        })
    };
    let request: Rc<dyn Fn()> = {
        let run = Rc::clone(&run);
        Rc::new(move || {
            let decision = gate.borrow_mut().request(now_ms());
            match decision {
                Decision::RunNow => run(),
                Decision::After(ms) => {
                    let (gate, run) = (Rc::clone(&gate), Rc::clone(&run));
                    set_timeout(
                        move || {
                            gate.borrow_mut().fire(now_ms());
                            run();
                        },
                        Duration::from_millis(u64::try_from(ms).unwrap_or(0)),
                    );
                }
                Decision::Scheduled => {}
            }
        })
    };
    request();
    Remote {
        data,
        error,
        request,
    }
}

#[cfg(test)]
mod tests {
    use super::retry_after_ms;

    #[test]
    fn a_retry_after_is_waited_in_milliseconds_and_never_overflows_the_timer() {
        assert_eq!(retry_after_ms(3), 3_000);
        assert_eq!(retry_after_ms(u64::MAX), u32::MAX);
    }
}
