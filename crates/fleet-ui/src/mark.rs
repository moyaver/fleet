// SPDX-License-Identifier: AGPL-3.0-only
//! The mark that goes with a status word: an icon and a colour, never colour alone (the
//! redesign's Foundations, rule 1).
//!
//! The icon is drawn in CSS (the `.st-*` classes in `tailwind.css`), so the text of a status is
//! its word and nothing else: `Passed`, `Not started`, and so on.

/// The icon and colour of a status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// A check on a filled disc: ran and passed, or online.
    Passed,
    /// A cross on a filled disc: ran and failed, or disconnected.
    Failed,
    /// A spinning ring: in progress now. Stops under reduced motion.
    Running,
    /// A thin ring: planned and waiting.
    NotStarted,
    /// An exclamation mark on a filled disc: cut short, or passed with a warning.
    Interrupted,
    /// A dashed ring: deliberately not run.
    Skipped,
    /// A question mark: no usable result. Never drawn as passed.
    Unknown,
    /// A square end mark: the job ran to its end. Never a check (invariant I3).
    End,
    /// A clock: no update recently, so the value is the last known one.
    Stale,
}

impl Mark {
    /// The CSS class that draws the icon and colours the word.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Passed => "st-pass",
            Self::Failed => "st-fail",
            Self::Running => "st-run",
            Self::NotStarted => "st-idle",
            Self::Interrupted => "st-warn",
            Self::Skipped => "st-skip",
            Self::Unknown => "st-unk",
            Self::End => "st-neutral",
            Self::Stale => "st-stale",
        }
    }
}
