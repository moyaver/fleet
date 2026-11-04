// SPDX-License-Identifier: AGPL-3.0-only
//! Toast queue view-model: at most N toasts show at once, the rest wait; each auto-dismisses
//! after a duration set by its tone; hovering or focusing the stack pauses every timer; an
//! identical message that is already queued only restarts its timer and bumps a count.

use crate::tone::Tone;

/// A toast's identity, stable for its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToastId(u64);

/// A toast waiting or showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Identity, for dismissal and keyed rendering.
    pub id: ToastId,
    /// The text, rendered as a text node.
    pub message: String,
    /// The colour role.
    pub tone: Tone,
    /// How many identical pushes this toast stands for (shown as "×N" when above one).
    pub count: u32,
    remaining_ms: u64,
}

impl Toast {
    /// Milliseconds left before this toast dismisses itself (while visible and unpaused).
    #[must_use]
    pub fn remaining_ms(&self) -> u64 {
        self.remaining_ms
    }

    /// The ARIA live role: errors interrupt (`alert`), everything else is polite (`status`).
    #[must_use]
    pub fn role(&self) -> &'static str {
        if self.tone == Tone::Danger {
            "alert"
        } else {
            "status"
        }
    }
}

/// How long a toast of this tone stays, in milliseconds. Problems stay longer than confirmations.
#[must_use]
pub const fn duration_ms(tone: Tone) -> u64 {
    match tone {
        Tone::Warning => 8_000,
        Tone::Danger => 12_000,
        Tone::Neutral | Tone::Info | Tone::Success | Tone::Accent | Tone::Muted => 5_000,
    }
}

/// The queue of toasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastQueue {
    max_visible: usize,
    toasts: Vec<Toast>,
    paused: bool,
    last_tick_ms: u64,
    next_id: u64,
}

impl ToastQueue {
    /// An empty queue showing at most `max_visible` toasts (at least one).
    #[must_use]
    pub fn new(max_visible: usize) -> Self {
        Self {
            max_visible: max_visible.max(1),
            toasts: Vec::new(),
            paused: false,
            last_tick_ms: 0,
            next_id: 0,
        }
    }

    /// Adds a toast at time `now_ms`, or refreshes the identical one already in the queue.
    pub fn push(&mut self, message: impl Into<String>, tone: Tone, now_ms: u64) -> ToastId {
        self.advance(now_ms);
        let message = message.into();
        if let Some(t) = self
            .toasts
            .iter_mut()
            .find(|t| t.message == message && t.tone == tone)
        {
            t.count = t.count.saturating_add(1);
            t.remaining_ms = duration_ms(tone);
            return t.id;
        }
        let id = ToastId(self.next_id);
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            message,
            tone,
            count: 1,
            remaining_ms: duration_ms(tone),
        });
        id
    }

    /// Removes a toast (the user closed it). Unknown ids are ignored.
    pub fn dismiss(&mut self, id: ToastId) {
        self.toasts.retain(|t| t.id != id);
    }

    /// Pauses or resumes every timer (pointer over or focus inside the stack).
    pub fn set_paused(&mut self, paused: bool, now_ms: u64) {
        self.advance(now_ms);
        self.paused = paused;
    }

    /// Advances the clock, dismissing visible toasts whose time ran out. Waiting toasts don't age.
    pub fn tick(&mut self, now_ms: u64) {
        self.advance(now_ms);
    }

    /// The toasts to show, oldest first.
    #[must_use]
    pub fn visible(&self) -> &[Toast] {
        self.toasts.get(..self.max_visible).unwrap_or(&self.toasts)
    }

    /// How many toasts wait for a free slot.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.toasts.len().saturating_sub(self.max_visible)
    }

    /// Whether timers are paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    fn advance(&mut self, now_ms: u64) {
        let dt = now_ms.saturating_sub(self.last_tick_ms);
        self.last_tick_ms = self.last_tick_ms.max(now_ms);
        if self.paused || dt == 0 {
            return;
        }
        // Only the toasts showing during `dt` age; one that frees a slot starts counting after.
        let mut left = dt;
        while left > 0 {
            let shown = self.max_visible.min(self.toasts.len());
            let Some(step) = self.toasts.iter().take(shown).map(|t| t.remaining_ms).min() else {
                return;
            };
            let step = step.min(left);
            for t in self.toasts.iter_mut().take(shown) {
                t.remaining_ms -= step;
            }
            left -= step;
            self.toasts.retain(|t| t.remaining_ms > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_by_tone() {
        assert_eq!(duration_ms(Tone::Success), 5_000);
        assert_eq!(duration_ms(Tone::Warning), 8_000);
        assert_eq!(duration_ms(Tone::Danger), 12_000);
    }

    #[test]
    fn a_toast_dismisses_itself_after_its_duration() {
        let mut q = ToastQueue::new(3);
        q.push("Saved", Tone::Success, 0);
        q.tick(4_999);
        assert_eq!(q.visible().len(), 1);
        q.tick(5_000);
        assert_eq!(q.visible(), []);
    }

    #[test]
    fn only_max_visible_show_and_the_rest_wait_without_aging() {
        let mut q = ToastQueue::new(2);
        q.push("a", Tone::Success, 0);
        q.push("b", Tone::Success, 0);
        q.push("c", Tone::Success, 0);
        assert_eq!(q.visible().len(), 2);
        assert_eq!(q.waiting(), 1);
        q.tick(5_000);
        let shown: Vec<_> = q.visible().iter().map(|t| t.message.as_str()).collect();
        assert_eq!(shown, ["c"]);
        assert_eq!(q.visible()[0].remaining_ms(), 5_000);
    }

    #[test]
    fn pausing_freezes_every_timer() {
        let mut q = ToastQueue::new(3);
        q.push("a", Tone::Success, 0);
        q.tick(2_000);
        q.set_paused(true, 2_000);
        q.tick(60_000);
        assert_eq!(q.visible().len(), 1);
        assert_eq!(q.visible()[0].remaining_ms(), 3_000);
        q.set_paused(false, 60_000);
        q.tick(62_999);
        assert_eq!(q.visible().len(), 1);
        q.tick(63_000);
        assert_eq!(q.visible(), []);
    }

    #[test]
    fn an_identical_toast_restarts_the_timer_and_counts() {
        let mut q = ToastQueue::new(3);
        let first = q.push("Connection lost", Tone::Danger, 0);
        let again = q.push("Connection lost", Tone::Danger, 10_000);
        assert_eq!(first, again);
        assert_eq!(q.visible().len(), 1);
        assert_eq!(q.visible()[0].count, 2);
        assert_eq!(q.visible()[0].remaining_ms(), 12_000);
    }

    #[test]
    fn same_text_with_another_tone_is_a_different_toast() {
        let mut q = ToastQueue::new(3);
        q.push("x", Tone::Info, 0);
        q.push("x", Tone::Warning, 0);
        assert_eq!(q.visible().len(), 2);
    }

    #[test]
    fn dismiss_removes_and_lets_a_waiting_toast_in() {
        let mut q = ToastQueue::new(1);
        let a = q.push("a", Tone::Info, 0);
        q.push("b", Tone::Info, 0);
        q.dismiss(a);
        assert_eq!(q.visible()[0].message, "b");
        q.dismiss(a);
        assert_eq!(q.visible().len(), 1);
    }

    #[test]
    fn errors_are_alerts_and_the_rest_status() {
        let mut q = ToastQueue::new(3);
        q.push("bad", Tone::Danger, 0);
        q.push("ok", Tone::Success, 0);
        let roles: Vec<_> = q.visible().iter().map(Toast::role).collect();
        assert_eq!(roles, ["alert", "status"]);
    }
}
