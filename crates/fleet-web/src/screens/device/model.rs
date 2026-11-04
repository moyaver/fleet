// SPDX-License-Identifier: AGPL-3.0-only
//! The device page's view-model: what `GET …/devices/{id}` says, as text and rows.
//!
//! Spec §8 screen C asks for model, OS, boot and connection status, the current shard and flow,
//! and previous flow outcomes. What the server doesn't know (a current step, a progress clock)
//! is shown as "Not reported" instead of being derived from something else.

use fleet_protocol::rest::{DeviceDetailView, DeviceProfileView, DeviceSessionView};
use fleet_ui::{charts::format::format_duration, provenance::Provenance, tone::Tone};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    paths::{device_href, flow_href, worker_href},
    screens::common::{Ctx, Fact},
    status::{Pill, flow_provenance, flow_state_pill},
};

/// A device session's state as a word and a colour role.
#[must_use]
pub const fn device_state_pill(state: &str) -> Pill {
    // `str` can't be matched with `==` in a const fn, so match the bytes of the fixed vocabulary.
    let (label, tone) = match state.as_bytes() {
        b"discovered" => ("Discovered", Tone::Neutral),
        b"booting" => ("Booting", Tone::Warning),
        b"ready" => ("Ready", Tone::Success),
        b"busy" => ("Busy", Tone::Info),
        b"offline" => ("Offline", Tone::Danger),
        b"released" => ("Released", Tone::Neutral),
        b"lost" => ("Lost", Tone::Danger),
        _ => ("Unrecognized state", Tone::Muted),
    };
    Pill { label, tone }
}

fn kind_text(p: &DeviceProfileView) -> String {
    let platform = match p.platform.as_str() {
        "android" => "Android",
        "ios" => "iOS",
        other => other,
    };
    let kind = match p.kind.as_str() {
        "physical" => "physical device",
        other => other,
    };
    format!("{platform} {kind}")
}

fn os_text(p: &DeviceProfileView) -> Option<String> {
    match (&p.os_version, p.api_level) {
        (Some(v), Some(api)) => Some(format!("{v} (API {api})")),
        (Some(v), None) => Some(v.clone()),
        (None, Some(api)) => Some(format!("API {api}")),
        (None, None) => None,
    }
}

/// The device at a glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHeader {
    /// The AVD or model, or the serial when the profile is unknown.
    pub title: String,
    /// The adb serial.
    pub subtitle: String,
    /// Boot and connection status.
    pub state: Pill,
    /// What it is.
    pub facts: Vec<Fact>,
    /// The worker's page, when the worker is known.
    pub worker_href: Option<String>,
}

impl DeviceHeader {
    /// The header of `view`, with times in the viewer's zone.
    #[must_use]
    pub fn build(view: &DeviceDetailView, ctx: &Ctx) -> Self {
        let s = &view.session;
        let profile = s.profile.as_ref();
        let title = profile
            .and_then(|p| p.avd_or_runtime.clone())
            .or_else(|| profile.and_then(|p| p.model.clone()))
            .unwrap_or_else(|| s.device.serial.clone());
        Self {
            title,
            subtitle: s.device.serial.clone(),
            state: device_state_pill(&s.device.state),
            facts: vec![
                Fact::maybe("Kind", profile.map(kind_text)),
                Fact::maybe("Model", profile.and_then(|p| p.model.clone())),
                Fact::maybe("OS version", profile.and_then(os_text)),
                Fact::maybe("CPU ABI", profile.and_then(|p| p.abi.clone())),
                Fact::maybe(
                    "Current shard",
                    view.flows
                        .first()
                        .and_then(|f| f.shard_number)
                        .map(|n| n.to_string()),
                ),
                Fact::known("Job", s.job_key.clone()),
                Fact::known("First seen", ctx.time(unix_ms(s.first_seen_at))),
                Fact::known("Last seen by the agent", ctx.ago(unix_ms(s.last_seen_at))),
            ],
            worker_href: s.worker_id.map(|w| worker_href(&ctx.project, w)),
        }
    }
}

/// The flow running on the device now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentFlow {
    /// The flow's name.
    pub name: String,
    /// Its page.
    pub href: String,
    /// How far it has got, as far as the server knows.
    pub steps_seen: String,
}

/// What the device is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionDetails {
    /// The running flow, if one is.
    pub current_flow: Option<CurrentFlow>,
    /// Facts about it the server may not have.
    pub facts: Vec<Fact>,
}

impl ExecutionDetails {
    /// The execution details in `view`.
    #[must_use]
    pub fn build(view: &DeviceDetailView, ctx: &Ctx) -> Self {
        let current_flow = view
            .flows
            .iter()
            .find(|f| f.state == "running")
            .map(|f| CurrentFlow {
                name: f.name.clone(),
                href: flow_href(&ctx.project, f.id),
                steps_seen: match f.steps {
                    1 => "1 step seen so far".to_owned(),
                    n => format!("{n} steps seen so far"),
                },
            });
        Self {
            current_flow,
            // The device view carries no step names: the flow page has them.
            facts: vec![Fact::maybe("Current step", None)],
        }
    }
}

/// One flow execution that ran on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRow {
    /// The flow's name.
    pub name: String,
    /// Its state.
    pub state: Pill,
    /// What the state rests on (invariant I4).
    pub provenance: Provenance,
    /// How settled it is, in words.
    pub finality_note: &'static str,
    /// Two parsed sources disagreed.
    pub has_conflict: bool,
    /// How long it took; `None` while it runs.
    pub duration: Option<String>,
    /// Steps seen.
    pub steps: i64,
    /// Why it failed, redacted.
    pub failure: Option<String>,
    /// Its page.
    pub href: String,
}

/// The flows that ran on the device, newest first, running ones first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRows {
    /// The rows.
    pub rows: Vec<FlowRow>,
    /// How many are shown of how many ran.
    pub summary: String,
}

impl FlowRows {
    /// The flow rows of `view`.
    #[must_use]
    pub fn build(view: &DeviceDetailView, ctx: &Ctx) -> Self {
        let rows: Vec<FlowRow> = view
            .flows
            .iter()
            .map(|f| FlowRow {
                name: f.name.clone(),
                state: flow_state_pill(&f.state),
                provenance: flow_provenance(&f.finality, &f.state, None),
                finality_note: crate::screens::common::finality_note(&f.finality),
                has_conflict: f.has_conflict,
                duration: f.duration_ms.map(format_duration),
                steps: f.steps,
                failure: f.failure_message.clone(),
                href: flow_href(&ctx.project, f.id),
            })
            .collect();
        let shown = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let summary = match (view.flow_count, shown) {
            (0, 0) => "No flow executions yet".to_owned(),
            (1, 1) => "1 flow execution".to_owned(),
            (total, n) if total <= n => format!("{n} flow executions"),
            (total, n) => format!("Showing the latest {n} of {total} flow executions"),
        };
        Self { rows, summary }
    }
}

/// One device session in a list, for the worker page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionRow {
    /// The session.
    pub id: Uuid,
    /// The adb serial.
    pub serial: String,
    /// Boot and connection status.
    pub state: Pill,
    /// `Pixel_7_API_34 · Android 14 (API 34) · arm64-v8a`, or `None` before the agent reports it.
    pub profile: Option<String>,
    /// The job it served.
    pub job_key: String,
    /// When the agent last saw it.
    pub last_seen: String,
    /// Its page.
    pub href: String,
}

impl DeviceSessionRow {
    /// The row for one session.
    #[must_use]
    pub fn build(s: &DeviceSessionView, ctx: &Ctx) -> Self {
        let profile = s.profile.as_ref().map(|p| {
            [
                p.avd_or_runtime.clone().or_else(|| p.model.clone()),
                os_text(p).map(|os| format!("{} {os}", platform_name(&p.platform))),
                p.abi.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
        });
        Self {
            id: s.device.id,
            serial: s.device.serial.clone(),
            state: device_state_pill(&s.device.state),
            profile: profile.filter(|p| !p.is_empty()),
            job_key: s.job_key.clone(),
            last_seen: ctx.ago(unix_ms(s.last_seen_at)),
            href: device_href(&ctx.project, s.device.id),
        }
    }
}

fn platform_name(p: &str) -> &str {
    match p {
        "android" => "Android",
        "ios" => "iOS",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = r#"{
      "session": {
        "device": {"id": "00000000-0000-0000-0000-0000000000d1",
                   "agent_run_id": "00000000-0000-0000-0000-000000000101",
                   "serial": "emulator-5554", "state": "busy", "rev": 4},
        "worker_id": "00000000-0000-0000-0000-0000000000a1", "runner_name": "mac-mini-3",
        "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
        "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "e2e-shard-1",
        "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:58Z",
        "profile": {"platform": "android", "kind": "emulator", "model": "sdk_gphone64_arm64",
                    "avd_or_runtime": "Pixel_7_API_34", "os_version": "14", "api_level": 34,
                    "abi": "arm64-v8a"}
      },
      "flows": [
        {"id": "00000000-0000-0000-0000-0000000000f3", "job_id": "00000000-0000-0000-0000-0000000000b1",
         "suite_execution_id": "00000000-0000-0000-0000-0000000000e1",
         "agent_run_id": "00000000-0000-0000-0000-000000000101",
         "device_session_id": "00000000-0000-0000-0000-0000000000d1",
         "shard_number": 1, "ordinal": 3, "name": "checkout", "path": "flows/checkout.yaml",
         "state": "running", "outcome_source": null, "finality": "provisional",
         "has_conflict": false, "failure_message": null, "duration_ms": null, "steps": 7, "rev": 2},
        {"id": "00000000-0000-0000-0000-0000000000f2", "job_id": "00000000-0000-0000-0000-0000000000b1",
         "suite_execution_id": "00000000-0000-0000-0000-0000000000e1",
         "agent_run_id": "00000000-0000-0000-0000-000000000101",
         "device_session_id": "00000000-0000-0000-0000-0000000000d1",
         "shard_number": 1, "ordinal": 2, "name": "login", "path": "flows/login.yaml",
         "state": "failed", "outcome_source": "live_console", "finality": "provisional",
         "has_conflict": false, "failure_message": "Element not found: Sign in",
         "duration_ms": 12400, "steps": 5, "rev": 3},
        {"id": "00000000-0000-0000-0000-0000000000f1", "job_id": "00000000-0000-0000-0000-0000000000b1",
         "suite_execution_id": "00000000-0000-0000-0000-0000000000e1",
         "agent_run_id": "00000000-0000-0000-0000-000000000101",
         "device_session_id": "00000000-0000-0000-0000-0000000000d1",
         "shard_number": 1, "ordinal": 1, "name": "onboarding", "path": null,
         "state": "passed", "outcome_source": "junit_final", "finality": "verified",
         "has_conflict": false, "failure_message": null, "duration_ms": 8000, "steps": 4, "rev": 2}
      ],
      "flow_count": 3
    }"#;

    fn ctx() -> Ctx {
        Ctx {
            project: "shop".to_owned(),
            now_ms: 1_767_265_200_000, // 2026-01-01T11:00:00Z
            utc_offset_minutes: 0,
        }
    }

    fn view() -> DeviceDetailView {
        serde_json::from_str(DEVICE).unwrap()
    }

    #[test]
    fn the_header_names_the_device_and_its_connection_status() {
        let h = DeviceHeader::build(&view(), &ctx());
        assert_eq!(h.title, "Pixel_7_API_34");
        assert_eq!(h.subtitle, "emulator-5554");
        assert_eq!((h.state.label, h.state.tone), ("Busy", Tone::Info));
        let facts: Vec<_> = h.facts.iter().map(|f| (f.label, f.shown())).collect();
        assert_eq!(
            facts,
            [
                ("Kind", "Android emulator"),
                ("Model", "sdk_gphone64_arm64"),
                ("OS version", "14 (API 34)"),
                ("CPU ABI", "arm64-v8a"),
                ("Current shard", "1"),
                ("Job", "e2e-shard-1"),
                ("First seen", "2026-01-01 10:00:00"),
                ("Last seen by the agent", "2 s ago"),
            ]
        );
        assert_eq!(
            h.worker_href.as_deref(),
            Some("/workers/00000000-0000-0000-0000-0000000000a1?project=shop")
        );
    }

    #[test]
    fn a_device_without_a_profile_says_so_and_falls_back_to_its_serial() {
        let mut v = view();
        v.session.profile = None;
        let h = DeviceHeader::build(&v, &ctx());
        assert_eq!(h.title, "emulator-5554");
        let fact = |label| {
            h.facts
                .iter()
                .find(|f| f.label == label)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(fact("Kind"), None);
        assert_eq!(fact("Model"), None);
        assert_eq!(fact("OS version"), None);
    }

    #[test]
    fn every_session_state_has_a_word_and_a_tone() {
        let expect = [
            ("discovered", "Discovered", Tone::Neutral),
            ("booting", "Booting", Tone::Warning),
            ("ready", "Ready", Tone::Success),
            ("busy", "Busy", Tone::Info),
            ("offline", "Offline", Tone::Danger),
            ("released", "Released", Tone::Neutral),
            ("lost", "Lost", Tone::Danger),
        ];
        for (state, label, tone) in expect {
            let p = device_state_pill(state);
            assert_eq!((p.label, p.tone), (label, tone), "{state}");
        }
        // A state this build doesn't know is shown as unrecognized, muted.
        let p = device_state_pill("hibernating");
        assert_eq!((p.label, p.tone), ("Unrecognized state", Tone::Muted));
    }

    #[test]
    fn the_current_flow_is_the_running_one_and_no_step_is_invented() {
        let e = ExecutionDetails::build(&view(), &ctx());
        let current = e.current_flow.expect("checkout is running");
        assert_eq!(current.name, "checkout");
        assert_eq!(
            current.href,
            "/flows/00000000-0000-0000-0000-0000000000f3?project=shop"
        );
        assert_eq!(current.steps_seen, "7 steps seen so far");
        // The device view carries no step names or progress clock.
        assert_eq!(
            e.facts
                .iter()
                .find(|f| f.label == "Current step")
                .unwrap()
                .value,
            None
        );
    }

    #[test]
    fn with_nothing_running_there_is_no_current_flow() {
        let mut v = view();
        v.flows.remove(0);
        assert_eq!(ExecutionDetails::build(&v, &ctx()).current_flow, None);
    }

    #[test]
    fn previous_flows_show_state_provenance_and_failure_with_a_link() {
        let rows = FlowRows::build(&view(), &ctx());
        let names: Vec<_> = rows.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["checkout", "login", "onboarding"]);
        let login = &rows.rows[1];
        assert_eq!(
            (login.state.label, login.state.tone),
            ("Failed", Tone::Danger)
        );
        // A live-console failure is provisional: never presented as final.
        assert_eq!(login.provenance, Provenance::Provisional);
        assert_eq!(login.duration.as_deref(), Some("12 s"));
        assert_eq!(login.failure.as_deref(), Some("Element not found: Sign in"));
        assert_eq!(
            login.href,
            "/flows/00000000-0000-0000-0000-0000000000f2?project=shop"
        );
        assert_eq!(rows.rows[2].provenance, Provenance::Final);
        // Running: the live view's weakest claim, no duration yet.
        assert_eq!(rows.rows[0].provenance, Provenance::Provisional);
        assert_eq!(rows.rows[0].duration, None);
        assert_eq!(rows.summary, "3 flow executions");
    }

    #[test]
    fn a_cut_list_says_how_many_are_not_shown() {
        let mut v = view();
        v.flow_count = 240;
        assert_eq!(
            FlowRows::build(&v, &ctx()).summary,
            "Showing the latest 3 of 240 flow executions"
        );
    }

    #[test]
    fn a_device_that_ran_nothing_is_an_empty_list_with_a_summary() {
        let mut v = view();
        v.flows.clear();
        v.flow_count = 0;
        let rows = FlowRows::build(&v, &ctx());
        assert_eq!(rows.rows.len(), 0);
        assert_eq!(rows.summary, "No flow executions yet");
    }
}
