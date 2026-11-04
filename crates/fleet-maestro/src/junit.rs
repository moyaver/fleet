// SPDX-License-Identifier: AGPL-3.0-only
//! Maestro's JUnit report (evidence lane E; research 05 §8.1–§8.2, research 08 Q9).
//!
//! One `<testsuite>` per shard, in shard order; `device=` is the AVD name. Maestro adds a
//! non-standard `status` attribute and writes no `<skipped/>`. A truncated file is salvaged:
//! every complete `<testcase>` before the cut is kept and the report is marked partial.

use fleet_protocol::event::{FlowOutcome, JunitTestcase, ReportStatus};
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};

/// One `<testsuite>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suite {
    /// `name`.
    pub name: Option<String>,
    /// `device`: the AVD name or device description.
    pub device: Option<String>,
    /// `tests`.
    pub tests: Option<u32>,
    /// `failures`.
    pub failures: Option<u32>,
    /// The test cases, in file order.
    pub cases: Vec<JunitTestcase>,
}

/// A parsed report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether it was read completely.
    pub status: ReportStatus,
    /// The suites read.
    pub suites: Vec<Suite>,
}

/// Maps Maestro's `status` attribute to an outcome. Unknown or missing is never `passed`.
#[must_use]
pub fn outcome(status: Option<&str>) -> FlowOutcome {
    match status {
        Some("SUCCESS" | "WARNING") => FlowOutcome::Passed,
        Some("ERROR") => FlowOutcome::Failed,
        Some("CANCELED" | "STOPPED") => FlowOutcome::Interrupted,
        _ => FlowOutcome::Unknown,
    }
}

/// Parses report bytes. Never fails: unreadable input is an `Invalid` report.
#[must_use]
pub fn parse(bytes: &[u8]) -> Report {
    let mut reader = Reader::from_reader(bytes);
    let mut suites: Vec<Suite> = Vec::new();
    let mut open_case: Option<JunitTestcase> = None;
    let mut in_failure = false;
    let mut saw_root = false;
    let mut buf = Vec::new();
    loop {
        let Ok(event) = reader.read_event_into(&mut buf) else {
            return finish(suites, saw_root, true);
        };
        match event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "testsuites" => {
                saw_root = true;
            }
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "testsuite" => {
                saw_root = true;
                suites.push(Suite {
                    name: attr(&e, "name"),
                    device: attr(&e, "device"),
                    tests: attr(&e, "tests").and_then(|v| v.parse().ok()),
                    failures: attr(&e, "failures").and_then(|v| v.parse().ok()),
                    cases: Vec::new(),
                });
            }
            Event::Start(e) if e.name().as_ref() == "testcase" => open_case = Some(testcase(&e)),
            Event::Empty(e) if e.name().as_ref() == "testcase" => {
                if let Some(s) = suites.last_mut() {
                    s.cases.push(testcase(&e));
                }
            }
            Event::Start(e) if e.name().as_ref() == "failure" => in_failure = true,
            Event::Text(t) if in_failure => {
                if let (Some(c), Ok(text)) = (
                    open_case.as_mut(),
                    Ok::<_, ()>(t.xml_content(XmlVersion::Implicit1_0)),
                ) {
                    c.failure.get_or_insert_with(String::new).push_str(&text);
                }
            }
            Event::GeneralRef(r) if in_failure => {
                if let (Some(c), Ok(Some(ch))) = (open_case.as_mut(), r.resolve_char_ref()) {
                    c.failure.get_or_insert_with(String::new).push(ch);
                } else if let (Some(c), Ok(name)) = (open_case.as_mut(), Ok::<_, ()>(&*r)) {
                    let ch = match name {
                        "lt" => "<",
                        "gt" => ">",
                        "amp" => "&",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    };
                    c.failure.get_or_insert_with(String::new).push_str(ch);
                }
            }
            Event::End(e) if e.name().as_ref() == "failure" => in_failure = false,
            Event::End(e) if e.name().as_ref() == "testcase" => {
                if let (Some(c), Some(s)) = (open_case.take(), suites.last_mut()) {
                    s.cases.push(c);
                }
            }
            Event::Eof => return finish(suites, saw_root, open_case.is_some()),
            _ => {}
        }
        buf.clear();
    }
}

fn finish(suites: Vec<Suite>, saw_root: bool, truncated: bool) -> Report {
    let status = match (saw_root, truncated) {
        (false, _) => ReportStatus::Invalid,
        (true, true) => ReportStatus::Partial,
        (true, false) => ReportStatus::Complete,
    };
    Report { status, suites }
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.try_get_attribute(name).ok().flatten().and_then(|a| {
        a.normalized_value(XmlVersion::Implicit1_0)
            .ok()
            .map(std::borrow::Cow::into_owned)
    })
}

fn testcase(e: &BytesStart<'_>) -> JunitTestcase {
    JunitTestcase {
        id: attr(e, "id"),
        name: attr(e, "name").unwrap_or_default(),
        file: attr(e, "file"),
        time_ms: attr(e, "time").and_then(|t| seconds_to_ms(&t)),
        timestamp_local: attr(e, "timestamp"),
        status: attr(e, "status"),
        failure: None,
    }
}

fn seconds_to_ms(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let whole: u64 = whole.parse().ok()?;
    let frac3: String = frac.chars().chain("000".chars()).take(3).collect();
    whole.checked_mul(1000)?.checked_add(frac3.parse().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPLIT: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<testsuites>
  <testsuite name="Test Suite" device="Pixel_10a_s3" tests="2" failures="1" time="29.676" timestamp="2026-11-04T02:32:25">
    <testcase id="bravo" name="bravo" classname="bravo" file="s/bravo.yaml" time="11.115" timestamp="2026-11-04T02:32:25" status="SUCCESS"/>
    <testcase id="d" name="delta-fails" classname="d" file="s/d.yaml" time="18.5" timestamp="2026-11-04T02:32:36" status="ERROR">
      <failure>[shard 1] Assertion is false: &quot;Fleet never shows this&quot; is visible</failure>
    </testcase>
  </testsuite>
  <testsuite name="Test Suite" device="Pixel_10a_s3" tests="1" failures="0"><testcase name="charlie" status="SUCCESS"/></testsuite>
</testsuites>"#;

    #[test]
    fn parses_a_sharded_report() {
        let r = parse(SPLIT.as_bytes());
        assert_eq!(r.status, ReportStatus::Complete);
        assert_eq!(r.suites.len(), 2);
        assert_eq!(r.suites[0].device.as_deref(), Some("Pixel_10a_s3"));
        let d = &r.suites[0].cases[1];
        assert_eq!(d.time_ms, Some(18_500));
        assert_eq!(
            d.failure.as_deref(),
            Some("[shard 1] Assertion is false: \"Fleet never shows this\" is visible")
        );
        assert_eq!(outcome(d.status.as_deref()), FlowOutcome::Failed);
        assert_eq!(r.suites[1].cases[0].name, "charlie");
    }

    #[test]
    fn salvages_a_truncated_report() {
        let cut = SPLIT.find("<testcase id=\"d\"").unwrap() + 30;
        let r = parse(&SPLIT.as_bytes()[..cut]);
        assert_eq!(r.status, ReportStatus::Partial);
        assert_eq!(r.suites[0].cases.len(), 1, "only the complete test case");
    }

    #[test]
    fn rejects_non_reports_and_maps_status() {
        assert_eq!(parse(b"").status, ReportStatus::Invalid);
        assert_eq!(parse(b"{\"json\":1}").status, ReportStatus::Invalid);
        assert_eq!(outcome(Some("WARNING")), FlowOutcome::Passed);
        assert_eq!(outcome(Some("RUNNING")), FlowOutcome::Unknown);
        assert_eq!(outcome(None), FlowOutcome::Unknown);
    }
}
