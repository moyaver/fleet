// SPDX-License-Identifier: AGPL-3.0-only
//! Provenance view-model (PLAN invariant I4, §10.1; spec §18): which label a value carries and
//! when fresh data turns stale. Wording follows spec §18 and research 05 §5.3.
//!
//! Provenance is monochrome: a pattern glyph and a word. Colour says how a result went (see
//! [`crate::mark`]); provenance says how well it is known, so a verified pass is not green.

/// How long without an update before data counts as stale, in milliseconds (research 05 §6.1).
pub const STALE_AFTER_MS: u64 = 25_000;

/// The label of a run that was published after the fact (issue #83). It is
/// not a provenance: a published run's values keep their own provenance, and this only says the
/// run was imported rather than observed live.
pub const PUBLISHED: Badge = Badge {
    label: "Published",
    aria_label: "Origin: published run",
    glyph: "pg-published",
    description: "Imported after the run ended, with fleet-agent publish. Nothing here was watched live.",
};

/// Where a displayed value comes from (spec §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// Seen directly by the agent: device state, exit code, signal, file existence.
    Observed,
    /// Read from live console output while the run is still open.
    Provisional,
    /// Confirmed by the final report.
    Final,
    /// Computed from other measurements (rollups).
    Derived,
    /// Fleet's explanation of something it did not see.
    Inferred,
    /// Nothing was received for this value.
    Missing,
    /// Not updated within the freshness threshold.
    Stale,
}

/// What a provenance label shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    /// Visible text.
    pub label: &'static str,
    /// Accessible name, so the meaning is not carried by the glyph alone.
    pub aria_label: &'static str,
    /// The CSS class of the monochrome pattern glyph (`pg-*` in `tailwind.css`).
    pub glyph: &'static str,
    /// Tooltip text explaining the provenance.
    pub description: &'static str,
}

impl Provenance {
    /// Whether a row or a single value carries this provenance's own mark. Settled results do not
    /// (issue #149): Verified, Observed and Derived are said once by the page, section or caption
    /// they belong to. Only the exceptions (Provisional, Inferred, Missing and Stale) are marked.
    #[must_use]
    pub const fn marks_a_row(self) -> bool {
        matches!(
            self,
            Self::Provisional | Self::Inferred | Self::Missing | Self::Stale
        )
    }

    /// The label for this provenance.
    #[must_use]
    pub const fn badge(self) -> Badge {
        match self {
            Self::Observed => Badge {
                label: "Observed",
                aria_label: "Provenance: observed",
                glyph: "pg-observed",
                description: "Seen directly by the agent on the runner, such as device state, exit code or files.",
            },
            Self::Provisional => Badge {
                label: "Provisional",
                aria_label: "Provenance: provisional",
                glyph: "pg-provisional",
                description: "Read from live console output while the run is open; the final report may differ.",
            },
            Self::Final => Badge {
                label: "Verified",
                aria_label: "Provenance: verified",
                glyph: "pg-verified",
                description: "Confirmed by the final test report; this result is settled.",
            },
            Self::Derived => Badge {
                label: "Derived",
                aria_label: "Provenance: derived",
                glyph: "pg-derived",
                description: "Computed from other measurements, not a raw reading.",
            },
            Self::Inferred => Badge {
                label: "Inferred",
                aria_label: "Provenance: inferred",
                glyph: "pg-inferred",
                description: "Fleet's explanation of what probably happened; it was not observed and is never shown as passed.",
            },
            Self::Missing => Badge {
                label: "Missing",
                aria_label: "Provenance: missing",
                glyph: "pg-missing",
                description: "Nothing was received for this value.",
            },
            Self::Stale => Badge {
                label: "Stale",
                aria_label: "Provenance: stale",
                glyph: "pg-stale",
                description: "No recent update; this is the last known value.",
            },
        }
    }

    /// The provenance to display for a value last updated `age_ms` ago: anything but
    /// [`Provenance::Missing`] turns [`Provenance::Stale`] once `age_ms` reaches `stale_after_ms`.
    #[must_use]
    pub const fn displayed(self, age_ms: u64, stale_after_ms: u64) -> Self {
        match self {
            Self::Missing => Self::Missing,
            _ if age_ms >= stale_after_ms => Self::Stale,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_use_the_spec_wording() {
        let b = Provenance::Final.badge();
        assert_eq!(b.label, "Verified");
        assert_eq!(b.aria_label, "Provenance: verified");
        assert_eq!(
            b.description,
            "Confirmed by the final test report; this result is settled."
        );
        assert_eq!(Provenance::Provisional.badge().label, "Provisional");
        assert_eq!(Provenance::Stale.badge().label, "Stale");
    }

    #[test]
    fn every_provenance_has_distinct_text_glyph_and_description() {
        let all = [
            Provenance::Observed,
            Provenance::Provisional,
            Provenance::Final,
            Provenance::Derived,
            Provenance::Inferred,
            Provenance::Missing,
            Provenance::Stale,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.badge().label, b.badge().label);
                assert_ne!(a.badge().aria_label, b.badge().aria_label);
                assert_ne!(a.badge().glyph, b.badge().glyph);
                assert_ne!(a.badge().description, b.badge().description);
            }
            assert!(a.badge().aria_label.starts_with("Provenance: "));
            assert!(a.badge().glyph.starts_with("pg-"));
        }
    }

    #[test]
    fn the_published_label_is_distinct_from_every_provenance() {
        let all = [
            Provenance::Observed,
            Provenance::Provisional,
            Provenance::Final,
            Provenance::Derived,
            Provenance::Inferred,
            Provenance::Missing,
            Provenance::Stale,
        ];
        for p in all {
            assert_ne!(p.badge().label, PUBLISHED.label);
            assert_ne!(p.badge().glyph, PUBLISHED.glyph);
        }
        assert!(PUBLISHED.aria_label.starts_with("Origin: "));
    }

    #[test]
    fn only_the_exceptions_carry_a_mark_on_a_row() {
        let marked: Vec<Provenance> = [
            Provenance::Observed,
            Provenance::Provisional,
            Provenance::Final,
            Provenance::Derived,
            Provenance::Inferred,
            Provenance::Missing,
            Provenance::Stale,
        ]
        .into_iter()
        .filter(|p| p.marks_a_row())
        .collect();
        assert_eq!(
            marked,
            [
                Provenance::Provisional,
                Provenance::Inferred,
                Provenance::Missing,
                Provenance::Stale
            ]
        );
    }

    #[test]
    fn data_turns_stale_at_the_threshold_but_missing_stays_missing() {
        assert_eq!(
            Provenance::Observed.displayed(STALE_AFTER_MS - 1, STALE_AFTER_MS),
            Provenance::Observed
        );
        assert_eq!(
            Provenance::Observed.displayed(25_000, STALE_AFTER_MS),
            Provenance::Stale
        );
        assert_eq!(
            Provenance::Missing.displayed(60_000, STALE_AFTER_MS),
            Provenance::Missing
        );
    }
}
