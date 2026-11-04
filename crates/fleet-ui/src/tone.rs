// SPDX-License-Identifier: AGPL-3.0-only
//! Semantic colour roles shared by badges, pills and stat tiles.

/// A semantic colour role. The colours behind each role are CSS custom properties, so light and
/// dark themes switch without touching components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain, no emphasis.
    Neutral,
    /// Informational.
    Info,
    /// Good, settled.
    Success,
    /// Needs attention.
    Warning,
    /// Failed or wrong.
    Danger,
    /// A machine-made explanation (inferred data).
    Accent,
    /// Greyed out: stale or missing.
    Muted,
}

impl Tone {
    /// The Tailwind classes for this role. Full literals, so Tailwind's scanner finds them.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Neutral => "bg-tone-neutral-bg text-tone-neutral-fg border-tone-neutral-fg/30",
            Self::Info => "bg-tone-info-bg text-tone-info-fg border-tone-info-fg/30",
            Self::Success => "bg-tone-success-bg text-tone-success-fg border-tone-success-fg/30",
            Self::Warning => "bg-tone-warning-bg text-tone-warning-fg border-tone-warning-fg/30",
            Self::Danger => "bg-tone-danger-bg text-tone-danger-fg border-tone-danger-fg/30",
            Self::Accent => "bg-tone-accent-bg text-tone-accent-fg border-tone-accent-fg/30",
            Self::Muted => "bg-tone-muted-bg text-tone-muted-fg border-tone-muted-fg/30",
        }
    }
}
