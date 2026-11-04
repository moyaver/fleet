// SPDX-License-Identifier: AGPL-3.0-only
//! Menu view-model (WAI-ARIA APG menu): the active item, arrow/Home/End navigation that skips
//! disabled items and wraps, and typeahead.

use crate::keyboard::Key;

/// How long a pause ends a typeahead search, in milliseconds.
pub const TYPEAHEAD_RESET_MS: u64 = 500;

/// One entry of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// The visible label (also what typeahead matches).
    pub label: String,
    /// Whether the item can't be activated.
    pub disabled: bool,
}

impl MenuItem {
    /// An enabled item.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disabled: false,
        }
    }

    /// The same item, disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }
}

/// What a key press did to the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent {
    /// The active item changed (or stayed); the view should move focus to it.
    Focus(usize),
    /// The user chose this item; the view runs its action and closes the popover.
    Activate(usize),
    /// The user asked to close the menu; the view closes it and returns focus to the trigger.
    Close,
    /// The key isn't used by menus; leave the event alone.
    Ignored,
}

/// A menu's items and which one is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    items: Vec<MenuItem>,
    active: Option<usize>,
    search: String,
    last_typed_ms: u64,
}

impl Menu {
    /// A menu with no active item.
    #[must_use]
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self {
            items,
            active: None,
            search: String::new(),
            last_typed_ms: 0,
        }
    }

    /// The items, in order.
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// The active item, if any.
    #[must_use]
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// The `tabindex` of an item: 0 for the active one, -1 for the rest (roving tabindex).
    #[must_use]
    pub fn tab_index(&self, index: usize) -> i32 {
        if self.active == Some(index) { 0 } else { -1 }
    }

    /// Records that the browser focused item `index` (a pointer or Tab moved focus). Disabled and
    /// out-of-range items are ignored.
    pub fn set_active(&mut self, index: usize) {
        if self.enabled(index) {
            self.active = Some(index);
        }
    }

    /// Opens the menu on its first (or last) enabled item and returns it.
    pub fn open(&mut self, last: bool) -> Option<usize> {
        self.search.clear();
        self.active = if last {
            self.last_enabled()
        } else {
            self.first_enabled()
        };
        self.active
    }

    /// Handles a key at time `now_ms` (milliseconds on any monotonic clock).
    pub fn on_key(&mut self, key: Key, now_ms: u64) -> MenuEvent {
        match key {
            Key::Down => self.move_to(self.step(true)),
            Key::Up => self.move_to(self.step(false)),
            Key::Home => self.move_to(self.first_enabled()),
            Key::End => self.move_to(self.last_enabled()),
            Key::Enter => match self.active {
                Some(i) if self.enabled(i) => MenuEvent::Activate(i),
                _ => MenuEvent::Ignored,
            },
            Key::Escape => MenuEvent::Close,
            Key::Char(c) => self.typeahead(c, now_ms),
            Key::Left | Key::Right | Key::Other => MenuEvent::Ignored,
        }
    }

    fn move_to(&mut self, target: Option<usize>) -> MenuEvent {
        match target {
            Some(i) => {
                self.active = Some(i);
                MenuEvent::Focus(i)
            }
            None => MenuEvent::Ignored,
        }
    }

    fn typeahead(&mut self, c: char, now_ms: u64) -> MenuEvent {
        if now_ms.saturating_sub(self.last_typed_ms) > TYPEAHEAD_RESET_MS {
            self.search.clear();
        }
        self.last_typed_ms = now_ms;
        self.search.extend(c.to_lowercase());
        let repeated = self
            .search
            .chars()
            .skip(1)
            .all(|s| self.search.starts_with(s));
        let n = self.items.len();
        // One repeated letter cycles through its matches starting after the active item; a longer
        // string matches by prefix starting at the active item.
        let (needle, skip) = if repeated {
            (self.search.chars().take(1).collect::<String>(), 1)
        } else {
            (self.search.clone(), 0)
        };
        let start = self.active.unwrap_or(n.saturating_sub(1));
        let skip = if self.active.is_none() { 1 } else { skip };
        let hit = (0..n).map(|d| (start + d + skip) % n.max(1)).find(|i| {
            self.enabled(*i)
                && self
                    .items
                    .get(*i)
                    .is_some_and(|it| it.label.to_lowercase().starts_with(&needle))
        });
        self.move_to(hit)
    }

    fn enabled(&self, i: usize) -> bool {
        self.items.get(i).is_some_and(|it| !it.disabled)
    }

    fn first_enabled(&self) -> Option<usize> {
        (0..self.items.len()).find(|i| self.enabled(*i))
    }

    fn last_enabled(&self) -> Option<usize> {
        (0..self.items.len()).rev().find(|i| self.enabled(*i))
    }

    fn step(&self, forward: bool) -> Option<usize> {
        let n = self.items.len();
        let Some(from) = self.active else {
            return if forward {
                self.first_enabled()
            } else {
                self.last_enabled()
            };
        };
        (1..=n)
            .map(|d| {
                if forward {
                    (from + d) % n
                } else {
                    (from + n - d % n) % n
                }
            })
            .find(|i| self.enabled(*i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        Menu::new(vec![
            MenuItem::new("Open"),
            MenuItem::new("Rename"),
            MenuItem::new("Retry").disabled(),
            MenuItem::new("Delete"),
            MenuItem::new("Restart"),
        ])
    }

    #[test]
    fn opening_focuses_the_first_or_last_enabled_item() {
        let mut m = menu();
        assert_eq!(m.open(false), Some(0));
        assert_eq!(m.open(true), Some(4));
        assert_eq!(m.tab_index(4), 0);
        assert_eq!(m.tab_index(0), -1);
    }

    #[test]
    fn arrows_skip_disabled_items_and_wrap() {
        let mut m = menu();
        m.open(false);
        assert_eq!(m.on_key(Key::Down, 0), MenuEvent::Focus(1));
        assert_eq!(m.on_key(Key::Down, 0), MenuEvent::Focus(3));
        assert_eq!(m.on_key(Key::Down, 0), MenuEvent::Focus(4));
        assert_eq!(m.on_key(Key::Down, 0), MenuEvent::Focus(0));
        assert_eq!(m.on_key(Key::Up, 0), MenuEvent::Focus(4));
        assert_eq!(m.on_key(Key::Home, 0), MenuEvent::Focus(0));
        assert_eq!(m.on_key(Key::End, 0), MenuEvent::Focus(4));
    }

    #[test]
    fn enter_activates_and_escape_closes() {
        let mut m = menu();
        m.open(false);
        m.on_key(Key::Down, 0);
        assert_eq!(m.on_key(Key::Enter, 0), MenuEvent::Activate(1));
        assert_eq!(m.on_key(Key::Escape, 0), MenuEvent::Close);
        assert_eq!(m.on_key(Key::Left, 0), MenuEvent::Ignored);
    }

    #[test]
    fn repeating_a_letter_cycles_through_its_matches() {
        let mut m = menu();
        m.open(false); // on "Open"
        assert_eq!(m.on_key(Key::Char('r'), 0), MenuEvent::Focus(1)); // Rename
        assert_eq!(m.on_key(Key::Char('r'), 100), MenuEvent::Focus(4)); // Retry is disabled
        assert_eq!(m.on_key(Key::Char('r'), 200), MenuEvent::Focus(1));
    }

    #[test]
    fn typing_a_prefix_narrows_the_match() {
        let mut m = menu();
        m.open(false);
        assert_eq!(m.on_key(Key::Char('r'), 0), MenuEvent::Focus(1));
        assert_eq!(m.on_key(Key::Char('e'), 100), MenuEvent::Focus(1));
        assert_eq!(m.on_key(Key::Char('s'), 200), MenuEvent::Focus(4)); // "res"
    }

    #[test]
    fn a_pause_resets_the_search() {
        let mut m = menu();
        m.open(false);
        m.on_key(Key::Char('r'), 0);
        m.on_key(Key::Char('e'), 100);
        // "s" after a pause is a fresh search for "s": nothing starts with it.
        assert_eq!(m.on_key(Key::Char('s'), 5_000), MenuEvent::Ignored);
        assert_eq!(m.active(), Some(1));
    }

    #[test]
    fn typeahead_is_case_insensitive_and_skips_disabled() {
        let mut m = menu();
        m.open(false);
        assert_eq!(m.on_key(Key::Char('D'), 0), MenuEvent::Focus(3));
    }

    #[test]
    fn an_empty_menu_ignores_everything() {
        let mut m = Menu::new(vec![]);
        assert_eq!(m.open(false), None);
        assert_eq!(m.on_key(Key::Down, 0), MenuEvent::Ignored);
        assert_eq!(m.on_key(Key::Char('a'), 0), MenuEvent::Ignored);
        assert_eq!(m.on_key(Key::Enter, 0), MenuEvent::Ignored);
    }
}
