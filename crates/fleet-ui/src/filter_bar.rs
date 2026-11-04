// SPDX-License-Identifier: AGPL-3.0-only
//! Filter bar view-model with URL-synced state. A [`FilterSpec`] names the filters a screen
//! offers; [`FilterSpec::decode`] reads them from a URL query string (invalid values and repeated
//! keys are ignored, unknown parameters are kept) and [`FilterSpec::encode`] writes them back in a
//! stable order: known filters in spec order, then the unknown parameters as they came.

use std::fmt::Write as _;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// What values a filter accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterKind {
    /// Any non-empty text.
    Text,
    /// One of a fixed set of values.
    Choice(Vec<String>),
    /// An RFC 3339 instant.
    Instant,
}

impl FilterKind {
    fn accepts(&self, value: &str) -> bool {
        match self {
            Self::Text => !value.is_empty(),
            Self::Choice(allowed) => allowed.iter().any(|a| a == value),
            Self::Instant => OffsetDateTime::parse(value, &Rfc3339).is_ok(),
        }
    }
}

/// One filter: its query parameter name and accepted values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterField {
    /// The URL query parameter.
    pub key: String,
    /// What it accepts.
    pub kind: FilterKind,
}

impl FilterField {
    /// A field named `key`.
    #[must_use]
    pub fn new(key: impl Into<String>, kind: FilterKind) -> Self {
        Self {
            key: key.into(),
            kind,
        }
    }
}

/// The filters a screen offers, in display and URL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec {
    fields: Vec<FilterField>,
}

/// The values the user chose, plus URL parameters this screen doesn't own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterState {
    /// Parallel to the spec's fields.
    values: Vec<Option<String>>,
    /// Unknown parameters, decoded, in original order.
    extra: Vec<(String, String)>,
}

impl FilterState {
    /// The value of filter `key`, if set.
    #[must_use]
    pub fn get<'a>(&'a self, spec: &FilterSpec, key: &str) -> Option<&'a str> {
        let i = spec.fields.iter().position(|f| f.key == key)?;
        self.values.get(i)?.as_deref()
    }

    /// Sets filter `key`; an empty value clears it. Returns `false` (and changes nothing) for an
    /// unknown key or a value the filter doesn't accept.
    pub fn set(&mut self, spec: &FilterSpec, key: &str, value: &str) -> bool {
        let Some(i) = spec.fields.iter().position(|f| f.key == key) else {
            return false;
        };
        let Some(slot) = self.values.get_mut(i) else {
            return false;
        };
        if value.is_empty() {
            *slot = None;
            true
        } else if spec.fields.get(i).is_some_and(|f| f.kind.accepts(value)) {
            *slot = Some(value.to_owned());
            true
        } else {
            false
        }
    }

    /// Clears every filter; unknown URL parameters stay.
    pub fn clear(&mut self) {
        self.values.iter_mut().for_each(|v| *v = None);
    }

    /// How many filters are set.
    #[must_use]
    pub fn active(&self) -> usize {
        self.values.iter().flatten().count()
    }
}

impl FilterSpec {
    /// A filter bar offering `fields`.
    #[must_use]
    pub fn new(fields: Vec<FilterField>) -> Self {
        Self { fields }
    }

    /// The fields, in order.
    #[must_use]
    pub fn fields(&self) -> &[FilterField] {
        &self.fields
    }

    /// An empty state for this spec.
    #[must_use]
    pub fn empty(&self) -> FilterState {
        FilterState {
            values: vec![None; self.fields.len()],
            extra: Vec::new(),
        }
    }

    /// Reads a query string (with or without the leading `?`). The first valid occurrence of a
    /// filter wins; invalid values are dropped; other parameters are kept for [`Self::encode`].
    #[must_use]
    pub fn decode(&self, query: &str) -> FilterState {
        let mut state = self.empty();
        for pair in query
            .trim_start_matches('?')
            .split('&')
            .filter(|p| !p.is_empty())
        {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let (k, v) = (percent_decode(k), percent_decode(v));
            match self.fields.iter().position(|f| f.key == k) {
                Some(i) => {
                    if let Some(slot) = state.values.get_mut(i)
                        && slot.is_none()
                        && self.fields.get(i).is_some_and(|f| f.kind.accepts(&v))
                    {
                        *slot = Some(v);
                    }
                }
                None => state.extra.push((k, v)),
            }
        }
        state
    }

    /// Writes the state as a query string without the leading `?`; empty when nothing is set.
    #[must_use]
    pub fn encode(&self, state: &FilterState) -> String {
        let known = self
            .fields
            .iter()
            .zip(&state.values)
            .filter_map(|(f, v)| v.as_ref().map(|v| (f.key.as_str(), v.as_str())));
        let extra = state.extra.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        known
            .chain(extra)
            .map(|(k, v)| {
                if v.is_empty() {
                    percent_encode(k)
                } else {
                    format!("{}={}", percent_encode(k), percent_encode(v))
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |o: usize| bytes.get(i + o).and_then(|c| char::from(*c).to_digit(16));
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                if let (Some(h), Some(l)) = (hex(1), hex(2)) {
                    out.push(u8::try_from(h * 16 + l).unwrap_or(b'%'));
                    i += 2;
                } else {
                    out.push(b);
                }
            }
            _ => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> FilterSpec {
        FilterSpec::new(vec![
            FilterField::new("q", FilterKind::Text),
            FilterField::new(
                "state",
                FilterKind::Choice(vec!["running".into(), "failed".into()]),
            ),
            FilterField::new("since", FilterKind::Instant),
        ])
    }

    #[test]
    fn decode_reads_known_filters() {
        let s = spec();
        let st = s.decode("?state=failed&q=login%20flow&since=2025-03-01T08%3A30%3A00Z");
        assert_eq!(st.get(&s, "q"), Some("login flow"));
        assert_eq!(st.get(&s, "state"), Some("failed"));
        assert_eq!(st.get(&s, "since"), Some("2025-03-01T08:30:00Z"));
        assert_eq!(st.active(), 3);
    }

    #[test]
    fn encode_orders_by_spec_not_by_input() {
        let s = spec();
        let st = s.decode("since=2025-03-01T08:30:00Z&state=failed&q=a+b");
        assert_eq!(
            s.encode(&st),
            "q=a%20b&state=failed&since=2025-03-01T08%3A30%3A00Z"
        );
    }

    #[test]
    fn invalid_values_are_ignored() {
        let s = spec();
        let st = s.decode("state=exploded&since=yesterday&q=");
        assert_eq!(st.active(), 0);
        assert_eq!(s.encode(&st), "");
    }

    #[test]
    fn unknown_parameters_survive_in_order_after_the_known_ones() {
        let s = spec();
        let st = s.decode("page=3&state=running&tab=logs&flag");
        assert_eq!(s.encode(&st), "state=running&page=3&tab=logs&flag");
    }

    #[test]
    fn the_first_valid_occurrence_wins() {
        let s = spec();
        let st = s.decode("state=bogus&state=failed&state=running");
        assert_eq!(st.get(&s, "state"), Some("failed"));
    }

    #[test]
    fn round_trips_awkward_text() {
        let s = spec();
        let mut st = s.empty();
        assert!(st.set(&s, "q", "a&b=c é+%"));
        let again = s.decode(&s.encode(&st));
        assert_eq!(again, st);
        assert_eq!(again.get(&s, "q"), Some("a&b=c é+%"));
    }

    #[test]
    fn set_validates_and_empty_clears() {
        let s = spec();
        let mut st = s.empty();
        assert!(!st.set(&s, "state", "exploded"));
        assert!(!st.set(&s, "nope", "x"));
        assert!(st.set(&s, "state", "running"));
        assert!(st.set(&s, "state", ""));
        assert_eq!(st.get(&s, "state"), None);
    }

    #[test]
    fn clear_keeps_foreign_parameters() {
        let s = spec();
        let mut st = s.decode("q=x&page=2");
        st.clear();
        assert_eq!(s.encode(&st), "page=2");
    }

    #[test]
    fn broken_percent_escapes_are_kept_literally() {
        let s = spec();
        let st = s.decode("q=100%&x=%zz");
        assert_eq!(st.get(&s, "q"), Some("100%"));
        assert_eq!(s.encode(&st), "q=100%25&x=%25zz");
    }
}
