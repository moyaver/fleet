// SPDX-License-Identifier: AGPL-3.0-only
//! The keys the overlay and form view-models react to, parsed from DOM `KeyboardEvent.key`.

/// A key press, reduced to what the kit's WAI-ARIA patterns use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `ArrowUp`.
    Up,
    /// `ArrowDown`.
    Down,
    /// `ArrowLeft`.
    Left,
    /// `ArrowRight`.
    Right,
    /// `Home`.
    Home,
    /// `End`.
    End,
    /// `Enter`.
    Enter,
    /// `Escape`.
    Escape,
    /// A single printable character (typeahead, typing).
    Char(char),
    /// Anything else; view-models ignore it.
    Other,
}

impl Key {
    /// Parses a DOM `KeyboardEvent.key` value. Unknown names and multi-character names map to
    /// [`Key::Other`]; a single character (not a space) is [`Key::Char`].
    #[must_use]
    pub fn from_dom(key: &str) -> Self {
        match key {
            "ArrowUp" => Self::Up,
            "ArrowDown" => Self::Down,
            "ArrowLeft" => Self::Left,
            "ArrowRight" => Self::Right,
            "Home" => Self::Home,
            "End" => Self::End,
            "Enter" => Self::Enter,
            "Escape" | "Esc" => Self::Escape,
            _ => {
                let mut chars = key.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_control() => Self::Char(c),
                    _ => Self::Other,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_key_names_map_to_keys() {
        assert_eq!(Key::from_dom("ArrowDown"), Key::Down);
        assert_eq!(Key::from_dom("Escape"), Key::Escape);
        assert_eq!(Key::from_dom("a"), Key::Char('a'));
        assert_eq!(Key::from_dom("Tab"), Key::Other);
        assert_eq!(Key::from_dom(""), Key::Other);
    }
}
