// SPDX-License-Identifier: AGPL-3.0-only
//! Date-time range view-model over two native `<input type="datetime-local">` fields. The inputs
//! hold wall-clock text in a display offset; the range's values are instants, exchanged as
//! RFC 3339 in UTC. Either end may be empty (open-ended); when both are set, start ≤ end.

use time::{
    OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339, macros::format_description,
};

/// Why a range isn't valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The start field's text isn't a date and time.
    #[error("start is not a valid date and time")]
    InvalidStart,
    /// The end field's text isn't a date and time.
    #[error("end is not a valid date and time")]
    InvalidEnd,
    /// The end is before the start.
    #[error("end is before start")]
    EndBeforeStart,
}

/// The two ends of a range and the offset the fields are shown in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateTimeRange {
    offset: UtcOffset,
    start: Result<Option<OffsetDateTime>, RangeError>,
    end: Result<Option<OffsetDateTime>, RangeError>,
}

impl DateTimeRange {
    /// An empty (open-ended both sides) range shown in `offset`.
    #[must_use]
    pub fn new(offset: UtcOffset) -> Self {
        Self {
            offset,
            start: Ok(None),
            end: Ok(None),
        }
    }

    /// The display offset, such as `UTC+02:00`, to show next to the fields.
    #[must_use]
    pub fn offset_label(&self) -> String {
        let (h, m, _) = self.offset.as_hms();
        if self.offset.is_utc() {
            "UTC".to_owned()
        } else {
            let sign = if self.offset.is_negative() { '-' } else { '+' };
            format!("UTC{sign}{:02}:{:02}", h.unsigned_abs(), m.unsigned_abs())
        }
    }

    /// Sets the start from a field's text (`YYYY-MM-DDTHH:MM`, seconds optional); empty clears it.
    pub fn set_start_text(&mut self, text: &str) {
        self.start = parse_local(text, self.offset).map_err(|()| RangeError::InvalidStart);
    }

    /// Sets the end from a field's text; empty clears it.
    pub fn set_end_text(&mut self, text: &str) {
        self.end = parse_local(text, self.offset).map_err(|()| RangeError::InvalidEnd);
    }

    /// Sets the start from an RFC 3339 instant (for example from the URL). Invalid text clears it.
    pub fn set_start_rfc3339(&mut self, value: &str) {
        self.start = Ok(OffsetDateTime::parse(value, &Rfc3339).ok());
    }

    /// Sets the end from an RFC 3339 instant. Invalid text clears it.
    pub fn set_end_rfc3339(&mut self, value: &str) {
        self.end = Ok(OffsetDateTime::parse(value, &Rfc3339).ok());
    }

    /// The text for the start input's `value`, in the display offset; empty if unset or invalid.
    #[must_use]
    pub fn start_text(&self) -> String {
        self.start
            .iter()
            .flatten()
            .next()
            .map_or_else(String::new, |t| format_local(*t, self.offset))
    }

    /// The text for the end input's `value`.
    #[must_use]
    pub fn end_text(&self) -> String {
        self.end
            .iter()
            .flatten()
            .next()
            .map_or_else(String::new, |t| format_local(*t, self.offset))
    }

    /// Every problem with the range, in field order; empty when valid.
    #[must_use]
    pub fn errors(&self) -> Vec<RangeError> {
        let mut errors = Vec::new();
        errors.extend(self.start.as_ref().err());
        errors.extend(self.end.as_ref().err());
        if let (Ok(Some(s)), Ok(Some(e))) = (&self.start, &self.end)
            && e < s
        {
            errors.push(RangeError::EndBeforeStart);
        }
        errors
    }

    /// The start as RFC 3339 in UTC (`…Z`), if set and the whole range is valid.
    #[must_use]
    pub fn start_rfc3339(&self) -> Option<String> {
        self.valid_value(&self.start)
    }

    /// The end as RFC 3339 in UTC, if set and the whole range is valid.
    #[must_use]
    pub fn end_rfc3339(&self) -> Option<String> {
        self.valid_value(&self.end)
    }

    fn valid_value(&self, v: &Result<Option<OffsetDateTime>, RangeError>) -> Option<String> {
        if !self.errors().is_empty() {
            return None;
        }
        let t = (*v.as_ref().ok()?)?;
        t.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()
    }
}

/// `Ok(None)` for empty text, `Ok(Some(instant))` for a valid one, `Err` otherwise.
fn parse_local(text: &str, offset: UtcOffset) -> Result<Option<OffsetDateTime>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let minutes = format_description!("[year]-[month]-[day]T[hour]:[minute]");
    let seconds = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    let parsed = time::PrimitiveDateTime::parse(text, &minutes)
        .or_else(|_| time::PrimitiveDateTime::parse(text, &seconds))
        .map_err(|_| ())?;
    Ok(Some(parsed.assume_offset(offset)))
}

fn format_local(t: OffsetDateTime, offset: UtcOffset) -> String {
    t.to_offset(offset)
        .format(format_description!("[year]-[month]-[day]T[hour]:[minute]"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plus2() -> UtcOffset {
        UtcOffset::from_hms(2, 0, 0).unwrap()
    }

    #[test]
    fn local_text_becomes_utc_rfc3339() {
        let mut r = DateTimeRange::new(plus2());
        r.set_start_text("2025-03-01T10:30");
        r.set_end_text("2025-03-01T12:00:15");
        assert_eq!(r.start_rfc3339().as_deref(), Some("2025-03-01T08:30:00Z"));
        assert_eq!(r.end_rfc3339().as_deref(), Some("2025-03-01T10:00:15Z"));
        assert_eq!(r.errors().len(), 0);
    }

    #[test]
    fn rfc3339_in_any_offset_shows_in_the_display_offset() {
        let mut r = DateTimeRange::new(plus2());
        r.set_start_rfc3339("2025-03-01T08:30:00Z");
        assert_eq!(r.start_text(), "2025-03-01T10:30");
        r.set_end_rfc3339("2025-03-01T05:00:00-05:00");
        assert_eq!(r.end_rfc3339().as_deref(), Some("2025-03-01T10:00:00Z"));
    }

    #[test]
    fn end_before_start_is_an_error_and_withholds_values() {
        let mut r = DateTimeRange::new(UtcOffset::UTC);
        r.set_start_text("2025-03-02T00:00");
        r.set_end_text("2025-03-01T00:00");
        assert_eq!(r.errors(), [RangeError::EndBeforeStart]);
        assert_eq!(r.start_rfc3339(), None);
    }

    #[test]
    fn equal_ends_are_valid() {
        let mut r = DateTimeRange::new(UtcOffset::UTC);
        r.set_start_text("2025-03-01T00:00");
        r.set_end_text("2025-03-01T00:00");
        assert_eq!(r.errors().len(), 0);
    }

    #[test]
    fn garbage_is_reported_per_field_and_empty_clears() {
        let mut r = DateTimeRange::new(UtcOffset::UTC);
        r.set_start_text("yesterday");
        r.set_end_text("2025-13-40T00:00");
        assert_eq!(
            r.errors(),
            [RangeError::InvalidStart, RangeError::InvalidEnd]
        );
        r.set_start_text("");
        r.set_end_text("");
        assert_eq!(r.errors().len(), 0);
        assert_eq!(r.start_rfc3339(), None);
    }

    #[test]
    fn one_open_end_is_valid() {
        let mut r = DateTimeRange::new(UtcOffset::UTC);
        r.set_end_text("2025-03-01T00:00");
        assert_eq!(r.errors().len(), 0);
        assert_eq!(r.start_rfc3339(), None);
        assert_eq!(r.end_rfc3339().as_deref(), Some("2025-03-01T00:00:00Z"));
    }

    #[test]
    fn offset_labels() {
        assert_eq!(DateTimeRange::new(UtcOffset::UTC).offset_label(), "UTC");
        assert_eq!(DateTimeRange::new(plus2()).offset_label(), "UTC+02:00");
        let west = UtcOffset::from_hms(-9, -30, 0).unwrap();
        assert_eq!(DateTimeRange::new(west).offset_label(), "UTC-09:30");
    }
}
