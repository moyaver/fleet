// SPDX-License-Identifier: AGPL-3.0-only
//! Combobox view-model (WAI-ARIA APG editable combobox with a list popup, manual selection):
//! typing filters the options, arrows move a virtual active option that the input announces
//! through `aria-activedescendant`, Enter picks it, Escape closes or clears, and leaving the
//! field selects only when the text exactly matches an option.

use crate::keyboard::Key;

/// One choice: a stable value and the text shown and matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboOption {
    /// The value reported on selection.
    pub value: String,
    /// The visible, matched text.
    pub label: String,
}

impl ComboOption {
    /// An option whose value differs from its label.
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// What a key press did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboEvent {
    /// State changed; the view should `preventDefault` so the caret doesn't move.
    Handled,
    /// The key isn't used in the current state (for example Enter with the list closed).
    Ignored,
}

/// The combobox state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combobox {
    id: String,
    options: Vec<ComboOption>,
    input: String,
    open: bool,
    /// Position in the filtered list.
    active: Option<usize>,
    /// Index into `options`.
    selected: Option<usize>,
}

impl Combobox {
    /// A closed, empty combobox. `id` prefixes the element ids (`{id}-listbox`, `{id}-opt-N`).
    #[must_use]
    pub fn new(id: impl Into<String>, options: Vec<ComboOption>) -> Self {
        Self {
            id: id.into(),
            options,
            input: String::new(),
            open: false,
            active: None,
            selected: None,
        }
    }

    /// The text in the input.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Whether the popup is showing (`aria-expanded`). An empty result list never shows.
    #[must_use]
    pub fn expanded(&self) -> bool {
        self.open && !self.filtered().is_empty()
    }

    /// The options whose label contains the input, case-insensitively, in original order. The
    /// text of the current selection shows everything, so reopening isn't a dead end.
    #[must_use]
    pub fn filtered(&self) -> Vec<&ComboOption> {
        let needle = self.input.to_lowercase();
        let showing_selection = self
            .selected
            .and_then(|i| self.options.get(i))
            .is_some_and(|o| o.label == self.input);
        self.options
            .iter()
            .filter(|o| showing_selection || o.label.to_lowercase().contains(&needle))
            .collect()
    }

    /// The selected option's value, if one is selected.
    #[must_use]
    pub fn selected_value(&self) -> Option<&str> {
        self.selected
            .and_then(|i| self.options.get(i))
            .map(|o| o.value.as_str())
    }

    /// The `id` of the popup (`aria-controls`).
    #[must_use]
    pub fn listbox_id(&self) -> String {
        format!("{}-listbox", self.id)
    }

    /// The `id` of the option at `position` in the filtered list.
    #[must_use]
    pub fn option_id(&self, position: usize) -> String {
        format!("{}-opt-{position}", self.id)
    }

    /// The id for `aria-activedescendant`: the active option while the popup shows.
    #[must_use]
    pub fn active_descendant(&self) -> Option<String> {
        self.active
            .filter(|_| self.expanded())
            .map(|p| self.option_id(p))
    }

    /// The active position in the filtered list (`aria-selected` on that option).
    #[must_use]
    pub fn active(&self) -> Option<usize> {
        self.active.filter(|_| self.expanded())
    }

    /// The user typed: filter, open, clear the active option, and drop a selection that no longer
    /// matches the text.
    pub fn set_input(&mut self, text: &str) {
        text.clone_into(&mut self.input);
        self.open = true;
        self.active = None;
        if self.selected_label() != Some(text) {
            self.selected = None;
        }
    }

    /// The user clicked the option at `position` in the filtered list.
    pub fn choose(&mut self, position: usize) {
        let Some(label) = self.filtered().get(position).map(|o| o.label.clone()) else {
            return;
        };
        self.selected = self.options.iter().position(|o| o.label == label);
        self.input = label;
        self.open = false;
        self.active = None;
    }

    /// Focus left the field: close, and select only when the text equals an option's label
    /// (ignoring case). A stale selection can't survive the text changing, so there is no revert.
    pub fn blur(&mut self) {
        self.open = false;
        self.active = None;
        if self.selected.is_none() {
            let lower = self.input.to_lowercase();
            if let Some(i) = self
                .options
                .iter()
                .position(|o| o.label.to_lowercase() == lower)
            {
                self.selected = Some(i);
                self.input.clone_from(&self.options[i].label);
            }
        }
    }

    /// Handles a key pressed in the input.
    pub fn on_key(&mut self, key: Key) -> ComboEvent {
        let count = self.filtered().len();
        match key {
            Key::Down | Key::Up if count == 0 => ComboEvent::Ignored,
            Key::Down => {
                self.active = Some(match (self.expanded(), self.active) {
                    (true, Some(p)) => (p + 1) % count,
                    _ => 0,
                });
                self.open = true;
                ComboEvent::Handled
            }
            Key::Up => {
                self.active = Some(match (self.expanded(), self.active) {
                    (true, Some(p)) => (p + count - 1) % count,
                    _ => count - 1,
                });
                self.open = true;
                ComboEvent::Handled
            }
            Key::Enter => match self.active().filter(|_| self.expanded()) {
                Some(p) => {
                    self.choose(p);
                    ComboEvent::Handled
                }
                None => ComboEvent::Ignored,
            },
            Key::Escape => {
                if self.expanded() {
                    self.open = false;
                    self.active = None;
                    ComboEvent::Handled
                } else if self.input.is_empty() {
                    ComboEvent::Ignored
                } else {
                    self.input.clear();
                    self.selected = None;
                    ComboEvent::Handled
                }
            }
            Key::Left | Key::Right | Key::Home | Key::End | Key::Char(_) | Key::Other => {
                ComboEvent::Ignored
            }
        }
    }

    fn selected_label(&self) -> Option<&str> {
        self.selected
            .and_then(|i| self.options.get(i))
            .map(|o| o.label.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo() -> Combobox {
        Combobox::new(
            "state",
            vec![
                ComboOption::new("run", "Running"),
                ComboOption::new("pass", "Passed"),
                ComboOption::new("fail", "Failed"),
                ComboOption::new("unk", "Uncertain"),
            ],
        )
    }

    fn labels(c: &Combobox) -> Vec<&str> {
        c.filtered().iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn typing_filters_case_insensitively_and_opens() {
        let mut c = combo();
        c.set_input("ed");
        assert!(c.expanded());
        assert_eq!(labels(&c), ["Passed", "Failed"]);
        assert_eq!(c.active_descendant(), None);
    }

    #[test]
    fn no_matches_means_no_popup() {
        let mut c = combo();
        c.set_input("zzz");
        assert!(!c.expanded());
    }

    #[test]
    fn arrows_move_the_active_option_and_expose_its_id() {
        let mut c = combo();
        c.set_input("ed");
        assert_eq!(c.on_key(Key::Down), ComboEvent::Handled);
        assert_eq!(c.active_descendant().as_deref(), Some("state-opt-0"));
        c.on_key(Key::Down);
        assert_eq!(c.active_descendant().as_deref(), Some("state-opt-1"));
        c.on_key(Key::Down);
        assert_eq!(c.active(), Some(0));
        c.on_key(Key::Up);
        assert_eq!(c.active(), Some(1));
        assert_eq!(c.listbox_id(), "state-listbox");
    }

    #[test]
    fn down_on_a_closed_list_opens_it_on_the_first_option_and_up_on_the_last() {
        let mut c = combo();
        c.on_key(Key::Down);
        assert!(c.expanded());
        assert_eq!(c.active(), Some(0));
        let mut d = combo();
        d.on_key(Key::Up);
        assert_eq!(d.active(), Some(3));
    }

    #[test]
    fn enter_selects_the_active_option_and_closes() {
        let mut c = combo();
        c.set_input("ed");
        c.on_key(Key::Down);
        c.on_key(Key::Down);
        assert_eq!(c.on_key(Key::Enter), ComboEvent::Handled);
        assert_eq!(c.selected_value(), Some("fail"));
        assert_eq!(c.input(), "Failed");
        assert!(!c.expanded());
    }

    #[test]
    fn enter_without_an_active_option_is_left_to_the_form() {
        let mut c = combo();
        c.set_input("ed");
        assert_eq!(c.on_key(Key::Enter), ComboEvent::Ignored);
        assert_eq!(c.selected_value(), None);
    }

    #[test]
    fn escape_closes_then_clears() {
        let mut c = combo();
        c.set_input("ed");
        assert_eq!(c.on_key(Key::Escape), ComboEvent::Handled);
        assert!(!c.expanded());
        assert_eq!(c.input(), "ed");
        assert_eq!(c.on_key(Key::Escape), ComboEvent::Handled);
        assert_eq!(c.input(), "");
        assert_eq!(c.on_key(Key::Escape), ComboEvent::Ignored);
    }

    #[test]
    fn blur_selects_only_on_an_exact_match() {
        let mut c = combo();
        c.set_input("pass");
        c.blur();
        assert_eq!(c.selected_value(), None);
        assert!(!c.expanded());

        c.set_input("passed");
        c.blur();
        assert_eq!(c.selected_value(), Some("pass"));
        assert_eq!(c.input(), "Passed");
    }

    #[test]
    fn editing_after_a_selection_drops_it() {
        let mut c = combo();
        c.set_input("Passed");
        c.on_key(Key::Down);
        c.on_key(Key::Enter);
        assert_eq!(c.selected_value(), Some("pass"));
        c.set_input("Passe");
        assert_eq!(c.selected_value(), None);
    }

    #[test]
    fn clicking_an_option_selects_it() {
        let mut c = combo();
        c.set_input("un"); // Running, Uncertain
        c.choose(1);
        assert_eq!(c.selected_value(), Some("unk"));
        assert_eq!(c.input(), "Uncertain");
    }

    #[test]
    fn reopening_after_a_selection_lists_everything() {
        let mut c = combo();
        c.set_input("Passed");
        c.on_key(Key::Down);
        c.on_key(Key::Enter);
        c.on_key(Key::Down);
        assert_eq!(c.filtered().len(), 4);
    }
}
