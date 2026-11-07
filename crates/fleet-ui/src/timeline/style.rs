// SPDX-License-Identifier: AGPL-3.0-only
//! How a bar looks: outcome and provenance in, tone, pattern and text out.
//!
//! Colour is never the only signal: provenance shows as a fill pattern and both are spelled out
//! in text for the table and for assistive technology (invariant I4). Tones and provenance are the kit's shared types.

use fleet_protocol::event::FlowOutcome;

use crate::provenance::Provenance;
use crate::timeline::model::BarState;
use crate::tone::Tone;

/// The fill of a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Settled.
    Solid,
    /// Diagonal stripes: not final yet.
    Hatched,
    /// Dashed outline, no fill: guessed, stale or missing.
    Dashed,
}

/// A bar's look and wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarStyle {
    /// The colour role.
    pub tone: Tone,
    /// The fill.
    pub pattern: Pattern,
    /// The status word: `Passed`, `Failed`, `Running`, …
    pub outcome: &'static str,
}

/// The style of a bar in `state` shown with `provenance`.
#[must_use]
pub fn bar_style(state: BarState, provenance: Provenance) -> BarStyle {
    let (outcome, tone) = match state {
        BarState::Running => ("Running", Tone::Info),
        BarState::Ended { outcome, .. } => match outcome {
            FlowOutcome::Passed => ("Passed", Tone::Success),
            FlowOutcome::Failed => ("Failed", Tone::Danger),
            FlowOutcome::Skipped => ("Skipped", Tone::Muted),
            FlowOutcome::Interrupted => ("Interrupted", Tone::Warning),
            FlowOutcome::Unknown | FlowOutcome::Unrecognized => ("Unknown", Tone::Neutral),
        },
    };
    let (tone, pattern) = match provenance {
        Provenance::Stale | Provenance::Missing => (Tone::Muted, Pattern::Dashed),
        Provenance::Inferred => (tone, Pattern::Dashed),
        Provenance::Provisional => (tone, Pattern::Hatched),
        Provenance::Observed | Provenance::Final | Provenance::Derived => (tone, Pattern::Solid),
    };
    BarStyle {
        tone,
        pattern,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(outcome: FlowOutcome) -> BarState {
        BarState::Ended { end_ms: 1, outcome }
    }

    #[test]
    fn tone_follows_the_outcome_when_the_data_is_fresh() {
        let tone = |s, p| bar_style(s, p).tone;
        let f = Provenance::Final;
        assert_eq!(tone(ended(FlowOutcome::Passed), f), Tone::Success);
        assert_eq!(tone(ended(FlowOutcome::Failed), f), Tone::Danger);
        assert_eq!(tone(ended(FlowOutcome::Interrupted), f), Tone::Warning);
        assert_eq!(tone(ended(FlowOutcome::Skipped), f), Tone::Muted);
        assert_eq!(tone(ended(FlowOutcome::Unknown), f), Tone::Neutral);
        assert_eq!(tone(ended(FlowOutcome::Unrecognized), f), Tone::Neutral);
        assert_eq!(tone(BarState::Running, Provenance::Observed), Tone::Info);
    }

    #[test]
    fn stale_and_missing_data_is_greyed_out_whatever_the_outcome() {
        for p in [Provenance::Stale, Provenance::Missing] {
            let s = bar_style(ended(FlowOutcome::Passed), p);
            assert_eq!((s.tone, s.pattern), (Tone::Muted, Pattern::Dashed));
            // The words still say what was last known.
            assert_eq!(s.outcome, "Passed");
        }
    }

    #[test]
    fn provenance_shows_as_a_pattern_not_only_as_a_colour() {
        let pattern = |p| bar_style(ended(FlowOutcome::Failed), p).pattern;
        assert_eq!(pattern(Provenance::Observed), Pattern::Solid);
        assert_eq!(pattern(Provenance::Final), Pattern::Solid);
        assert_eq!(pattern(Provenance::Derived), Pattern::Solid);
        assert_eq!(pattern(Provenance::Provisional), Pattern::Hatched);
        assert_eq!(pattern(Provenance::Inferred), Pattern::Dashed);
    }

    #[test]
    fn outcome_words() {
        let word = |s| bar_style(s, Provenance::Final).outcome;
        assert_eq!(word(BarState::Running), "Running");
        assert_eq!(word(ended(FlowOutcome::Passed)), "Passed");
        assert_eq!(word(ended(FlowOutcome::Failed)), "Failed");
        assert_eq!(word(ended(FlowOutcome::Interrupted)), "Interrupted");
        assert_eq!(word(ended(FlowOutcome::Skipped)), "Skipped");
        assert_eq!(word(ended(FlowOutcome::Unknown)), "Unknown");
        assert_eq!(word(ended(FlowOutcome::Unrecognized)), "Unknown");
    }
}
