// SPDX-License-Identifier: AGPL-3.0-only
//! Run-level announcements for the live pages (issue #149): one batched, polite status region per
//! page, read out at most once per [`GAP_MS`].
//!
//! A page reports its run-level facts (a run started or ended, a flow failed, a verdict changed)
//! from the data it already holds. A fact is announced once, when it first appears after the page
//! has loaded; the facts that arrive between two read-outs go out together. Log lines, cell
//! updates and the ticking clock never reach the region, so a busy run stays quiet.

use std::{collections::HashSet, time::Duration};

use leptos::prelude::*;

use crate::clock::now_ms;

/// The shortest time between two read-outs, in milliseconds.
pub const GAP_MS: i64 = 5_000;

/// A run-level fact a page can announce. `key` names the state reached, so the same state is
/// announced once however often the page sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// Identifies the state, such as `run:<id>:started`.
    pub key: String,
    /// What is read out, such as "Run login started.".
    pub text: String,
}

impl Fact {
    /// A fact for `key`, read out as `text`.
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
        }
    }
}

/// The batching rule, free of the browser: what is new since the last read-out, and when a
/// read-out is due.
#[derive(Debug, Default)]
pub struct Batcher {
    /// The keys seen so far; `None` until the page has loaded.
    known: Option<HashSet<String>>,
    /// Texts of new facts waiting for the next read-out, in arrival order.
    pending: Vec<String>,
    /// When the last read-out happened.
    last_read_out: Option<i64>,
}

impl Batcher {
    /// Takes the facts the page shows now. The first facts seen are the page's state at load and
    /// are not announced; later facts with a new key are queued.
    pub fn observe(&mut self, facts: &[Fact]) {
        match self.known.as_mut() {
            None => self.known = Some(facts.iter().map(|f| f.key.clone()).collect()),
            Some(known) => {
                for fact in facts {
                    if known.insert(fact.key.clone()) {
                        self.pending.push(fact.text.clone());
                    }
                }
            }
        }
    }

    /// The batch to read out at `now_ms`, if there is one and [`GAP_MS`] has passed since the
    /// last read-out. The batch is every queued text, joined with spaces.
    pub fn due(&mut self, now_ms: i64) -> Option<String> {
        let gap_passed = self.last_read_out.is_none_or(|at| now_ms - at >= GAP_MS);
        if self.pending.is_empty() || !gap_passed {
            return None;
        }
        self.last_read_out = Some(now_ms);
        Some(std::mem::take(&mut self.pending).join(" "))
    }
}

/// The text of a page's status region. `facts` is `None` until the page has loaded; the facts it
/// holds then are the baseline. The text changes at most once per [`GAP_MS`], and only when a new
/// fact has arrived.
#[must_use]
pub fn region(facts: Signal<Option<Vec<Fact>>>) -> Signal<String> {
    let batcher = RwSignal::new_local(Batcher::default());
    let text = RwSignal::new(String::new());
    Effect::new(move |_| {
        if let Some(facts) = facts.get() {
            batcher.update(|b| b.observe(&facts));
        }
    });
    if let Ok(handle) = set_interval_with_handle(
        move || {
            let now = now_ms();
            if let Some(batch) = batcher.try_update(|b| b.due(now)).flatten() {
                text.set(batch);
            }
        },
        Duration::from_secs(1),
    ) {
        on_cleanup(move || handle.clear());
    }
    text.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(key: &str) -> Fact {
        Fact::new(key, format!("{key} happened."))
    }

    #[test]
    fn the_facts_a_page_loads_with_are_its_baseline_and_are_not_read_out() {
        let mut b = Batcher::default();
        b.observe(&[fact("run:1:started")]);
        assert_eq!(b.due(0), None, "loading is not an event");
    }

    #[test]
    fn a_new_fact_is_read_out_once_and_never_again() {
        let mut b = Batcher::default();
        b.observe(&[]);
        b.observe(&[fact("run:1:started")]);
        assert_eq!(b.due(0).as_deref(), Some("run:1:started happened."));
        b.observe(&[fact("run:1:started")]);
        assert_eq!(
            b.due(GAP_MS * 10),
            None,
            "the same state is not announced twice"
        );
    }

    #[test]
    fn facts_that_arrive_together_go_out_as_one_batch_and_the_next_waits_for_the_gap() {
        let mut b = Batcher::default();
        b.observe(&[]);
        b.observe(&[fact("a"), fact("b"), fact("c")]);
        assert_eq!(
            b.due(1_000).as_deref(),
            Some("a happened. b happened. c happened.")
        );

        b.observe(&[fact("d")]);
        assert_eq!(
            b.due(1_000 + GAP_MS - 1),
            None,
            "inside the gap nothing is read out"
        );
        assert_eq!(b.due(1_000 + GAP_MS).as_deref(), Some("d happened."));
    }

    #[test]
    fn a_burst_of_events_within_the_gap_is_one_read_out() {
        let mut b = Batcher::default();
        b.observe(&[]);
        let mut read_outs = 0;
        for tick in 0..30_i64 {
            let keys: Vec<Fact> = (0..=tick).map(|i| fact(&format!("flow:{i}"))).collect();
            b.observe(&keys);
            if b.due(tick * 1_000).is_some() {
                read_outs += 1;
            }
        }
        // Thirty seconds of events: at most one read-out per five seconds, so at most seven.
        assert!(read_outs <= 7, "{read_outs} read-outs in 30 s");
        assert!(read_outs >= 5, "the events are still read out: {read_outs}");
    }
}
