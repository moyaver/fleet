// SPDX-License-Identifier: AGPL-3.0-only
//! Tabs view-model (WAI-ARIA APG tabs, automatic activation, horizontal): selection, roving
//! tabindex and arrow/Home/End navigation that skips disabled tabs and wraps around.

use crate::keyboard::Key;

/// Which tab is selected, and which tabs can't be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tabs {
    disabled: Vec<bool>,
    selected: usize,
}

impl Tabs {
    /// A tab list with `count` enabled tabs and the first one selected.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            disabled: vec![false; count],
            selected: 0,
        }
    }

    /// Marks tab `index` as disabled. The selection moves to the next enabled tab if it was on it.
    #[must_use]
    pub fn with_disabled(mut self, index: usize) -> Self {
        if let Some(d) = self.disabled.get_mut(index) {
            *d = true;
        }
        if self.is_disabled(self.selected)
            && let Some(next) = self.step(self.selected, true)
        {
            self.selected = next;
        }
        self
    }

    /// The selected tab (`aria-selected="true"`).
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Whether tab `index` is disabled (or out of range).
    #[must_use]
    pub fn is_disabled(&self, index: usize) -> bool {
        self.disabled.get(index).copied().unwrap_or(true)
    }

    /// The `tabindex` of tab `index`: 0 for the selected tab, -1 for the rest (roving tabindex).
    #[must_use]
    pub fn tab_index(&self, index: usize) -> i32 {
        if index == self.selected { 0 } else { -1 }
    }

    /// Selects tab `index` (a click). Disabled and out-of-range tabs are ignored.
    pub fn select(&mut self, index: usize) {
        if !self.is_disabled(index) {
            self.selected = index;
        }
    }

    /// Handles a key on the tab list. Returns the tab that should now take focus, or `None` if
    /// the key isn't one the pattern uses (so the event is left alone). Selection follows focus.
    pub fn on_key(&mut self, key: Key) -> Option<usize> {
        let target = match key {
            Key::Right => self.step(self.selected, true),
            Key::Left => self.step(self.selected, false),
            Key::Home => (0..self.disabled.len()).find(|i| !self.is_disabled(*i)),
            Key::End => (0..self.disabled.len())
                .rev()
                .find(|i| !self.is_disabled(*i)),
            _ => return None,
        }?;
        self.selected = target;
        Some(target)
    }

    /// The next enabled tab after `from` in one direction, wrapping; `None` if there is none.
    fn step(&self, from: usize, forward: bool) -> Option<usize> {
        let n = self.disabled.len();
        (1..=n)
            .map(|d| {
                if forward {
                    (from + d) % n
                } else {
                    (from + n - d % n) % n
                }
            })
            .find(|i| !self.is_disabled(*i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_tab_is_the_only_tab_stop() {
        let tabs = Tabs::new(3);
        assert_eq!(
            [tabs.tab_index(0), tabs.tab_index(1), tabs.tab_index(2)],
            [0, -1, -1]
        );
    }

    #[test]
    fn arrows_move_focus_and_selection_and_wrap() {
        let mut tabs = Tabs::new(3);
        assert_eq!(tabs.on_key(Key::Right), Some(1));
        assert_eq!(tabs.selected(), 1);
        assert_eq!(tabs.on_key(Key::Right), Some(2));
        assert_eq!(tabs.on_key(Key::Right), Some(0));
        assert_eq!(tabs.on_key(Key::Left), Some(2));
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut tabs = Tabs::new(4);
        assert_eq!(tabs.on_key(Key::End), Some(3));
        assert_eq!(tabs.on_key(Key::Home), Some(0));
    }

    #[test]
    fn disabled_tabs_are_skipped_and_never_selected() {
        let mut tabs = Tabs::new(4).with_disabled(1).with_disabled(3);
        assert_eq!(tabs.on_key(Key::Right), Some(2));
        assert_eq!(tabs.on_key(Key::Right), Some(0));
        assert_eq!(tabs.on_key(Key::End), Some(2));
        tabs.select(1);
        assert_eq!(tabs.selected(), 2);
    }

    #[test]
    fn disabling_the_selected_tab_moves_the_selection() {
        let tabs = Tabs::new(3).with_disabled(0);
        assert_eq!(tabs.selected(), 1);
    }

    #[test]
    fn unrelated_keys_are_not_handled() {
        let mut tabs = Tabs::new(3);
        assert_eq!(tabs.on_key(Key::Down), None);
        assert_eq!(tabs.on_key(Key::Char('x')), None);
        assert_eq!(tabs.selected(), 0);
    }

    #[test]
    fn a_single_tab_stays_put() {
        let mut tabs = Tabs::new(1);
        assert_eq!(tabs.on_key(Key::Right), Some(0));
        let mut none = Tabs::new(0);
        assert_eq!(none.on_key(Key::Right), None);
    }
}
