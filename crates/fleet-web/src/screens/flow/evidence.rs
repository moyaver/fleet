// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D's evidence (PLAN §12 M5 exit, spec §16 criterion 8): what a flow left behind, laid
//! out so a failure can be diagnosed after the runner is gone.
//!
//! The artifacts REST (`GET …/flows/{id}/artifacts`) names each file and gives a short-lived
//! signed link on the usercontent origin. This module decides what to show and where each file
//! goes, and reads the two text files the page fetches from that origin itself:
//!
//! - [`Evidence`]: the screenshots for the viewer, the log and `commands.json` to fetch, the
//!   Maestro HTML report to link, the other files to offer as downloads, and refused uploads;
//! - [`LogEvidence`]: the redacted `maestro.log` as a [`LogModel`], and the line where each step
//!   of this flow starts, so the viewer can jump to the failing step;
//! - [`command_rows`]: step timings from `commands.json`.
//!
//! Nothing is invented: a flow that uploaded nothing, a server that cannot open files and a
//! refused upload each get their own wording.

use fleet_protocol::{
    artifact::{ArtifactClass, RejectReason},
    rest::{ArtifactListView, ArtifactView},
};
use fleet_ui::{
    charts::format::{Unit, format_duration},
    logview::model::{LogModel, StepStart},
    mark::Mark,
    screenshots::Shot,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{api::ApiError, clock::unix_ms};

/// The longest log the page fetches and parses (the whole text is held in the browser).
pub const MAX_LOG_BYTES: i64 = 8 * 1024 * 1024;

/// Links are renewed this long before the earliest one expires.
const RENEW_BEFORE_MS: i64 = 60_000;

/// A failed refresh that may pass (no network, a server error) is tried again this soon, while
/// the links it failed to renew are still held.
const RETRY_AFTER_FAILURE_MS: i64 = 5_000;

/// A fetch of a text file, for the log and `commands.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load<T> {
    /// On its way.
    Loading,
    /// Here.
    Ready(T),
    /// It could not be read; the text says why.
    Failed(String),
}

/// A step of the flow, as the page knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepKey {
    /// The step tree's id for it (its index, as text).
    pub id: String,
    /// Maestro's description of it, when the server has one.
    pub label: Option<String>,
}

/// A file that opens on the usercontent origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLink {
    /// What it is, in words (`Maestro log`).
    pub kind: &'static str,
    /// File name.
    pub name: String,
    /// Size, with its unit.
    pub size: String,
    /// The signed URL.
    pub href: String,
}

/// A text file the page fetches from the usercontent origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    /// The artifact (the page fetches again only when this changes, not when the link does).
    pub id: Uuid,
    /// File name.
    pub name: String,
    /// The signed URL now.
    pub url: String,
}

/// What to do with the Maestro log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSource {
    /// Fetch it and show it in the log viewer.
    Show(TextFile),
    /// Too big to hold in the browser: offer the download.
    TooLarge(EvidenceLink),
}

/// An upload the server refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// File name.
    pub name: String,
    /// What it was meant to be.
    pub kind: &'static str,
    /// Why, in words.
    pub reason: String,
}

/// A step's UI hierarchy, to save, shown under the step it was taken at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hierarchy {
    /// The step's id in the step tree (its index, as text).
    pub step: String,
    /// The step's description, when the flow's steps have one.
    pub step_label: Option<String>,
    /// Whether this is the step the failure names.
    pub failed: bool,
    /// The file.
    pub link: EvidenceLink,
}

/// What Maestro's own screenshot or recording is (issue #133).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// A `takeScreenshot` image: shown inline.
    Screenshot,
    /// A `startRecording` video: played in the page's own player.
    Recording,
}

/// A screenshot or recording the flow took on purpose, shown on the step that made it (issue #133).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnMedia {
    /// The artifact (the page keys its player by it, so a renewed link updates it in place).
    pub id: Uuid,
    /// What it is.
    pub kind: MediaKind,
    /// The step that made it, in the step tree's ids. `None` when the upload names no step.
    pub step: Option<String>,
    /// The step's description, when the flow's steps have one.
    pub step_label: Option<String>,
    /// File name.
    pub name: String,
    /// Size, with its unit.
    pub size: String,
    /// The signed URL now.
    pub url: String,
}

/// Everything there is to show for a flow whose evidence can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceFiles {
    /// Failure screenshots in capture order, each tied to the step it was taken at.
    pub screenshots: Vec<Shot>,
    /// Maestro's own screenshots and recordings of a failed flow, in capture order (issue #133).
    pub own_media: Vec<OwnMedia>,
    /// UI hierarchies of failing steps, in capture order.
    pub hierarchies: Vec<Hierarchy>,
    /// The Maestro log, when one was uploaded.
    pub log: Option<LogSource>,
    /// The device logcat excerpt, when one was uploaded.
    pub logcat: Option<LogSource>,
    /// `commands.json`, when one was uploaded.
    pub commands: Option<TextFile>,
    /// Maestro's HTML report, which opens on the usercontent origin, sandboxed.
    pub report: Option<EvidenceLink>,
    /// The other available files, to save.
    pub downloads: Vec<EvidenceLink>,
    /// Uploads the server refused.
    pub refused: Vec<Refused>,
}

/// How a text log (the Maestro log or the device logcat) is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMount {
    /// No log was uploaded.
    None,
    /// In the log viewer.
    Viewer,
    /// Too big for the browser: a download link.
    Download,
}

/// Which parts of the evidence card a set of files mounts. A renewed link changes none of it, so
/// the card's viewers stay mounted, with their state, while the links are renewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mounts {
    /// Whether there are failure screenshots for the viewer.
    pub screenshots: bool,
    /// How the log is shown.
    pub log: LogMount,
    /// How the device logcat is shown.
    pub logcat: LogMount,
    /// Whether there is a `commands.json` to show as timings.
    pub commands: bool,
}

/// How a text log is shown, by what the files hold.
fn mount_of(source: Option<&LogSource>) -> LogMount {
    match source {
        None => LogMount::None,
        Some(LogSource::Show(_)) => LogMount::Viewer,
        Some(LogSource::TooLarge(_)) => LogMount::Download,
    }
}

impl EvidenceFiles {
    /// What the card mounts for these files.
    #[must_use]
    pub fn mounts(&self) -> Mounts {
        Mounts {
            screenshots: !self.screenshots.is_empty(),
            log: mount_of(self.log.as_ref()),
            logcat: mount_of(self.logcat.as_ref()),
            commands: self.commands.is_some(),
        }
    }
}

/// What screen D says about a flow's evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// The list has not arrived yet.
    Loading,
    /// The list could not be read; the text says why.
    Unavailable(String),
    /// The run uploaded nothing for this flow; the text says what that can mean.
    Nothing(&'static str),
    /// Files are stored but this server cannot hand out links to them.
    Unopenable(&'static str),
    /// Files to show.
    Files(Box<EvidenceFiles>),
}

impl Evidence {
    /// Lays out `list` for the flow `flow`. `failed_step` is the step the failure names, `steps`
    /// all of the flow's steps (a screenshot's file name says which step took it).
    #[must_use]
    pub fn build(
        list: &ArtifactListView,
        flow: Uuid,
        failed_step: Option<&StepKey>,
        steps: &[StepKey],
    ) -> Self {
        let mine: Vec<&ArtifactView> = list
            .artifacts
            .iter()
            .filter(|a| a.flow_execution_id.is_none_or(|f| f == flow))
            .collect();
        if mine.is_empty() {
            return Self::Nothing(
                "No evidence was uploaded for this flow. A run from a fork pull request, or one \
                 whose agent had no Fleet credential or uploads turned off, uploads nothing; so \
                 does a flow that never reached the point of leaving a screenshot or log.",
            );
        }
        let available: Vec<&ArtifactView> = mine
            .iter()
            .copied()
            .filter(|a| a.state == "available")
            .collect();
        if !available.is_empty() && available.iter().all(|a| a.link.is_none()) {
            return Self::Unopenable(
                "The evidence is stored, but this server has no usercontent origin configured, so \
                 it cannot be opened from here. The operator guide explains how to set one up.",
            );
        }
        let mut files = EvidenceFiles {
            screenshots: Vec::new(),
            own_media: Vec::new(),
            hierarchies: Vec::new(),
            log: None,
            logcat: None,
            commands: None,
            report: None,
            downloads: Vec::new(),
            refused: Vec::new(),
        };
        for a in mine {
            if a.state != "available" {
                files.refused.push(Refused {
                    name: a.name.clone(),
                    kind: kind(ArtifactClass::parse(&a.class)),
                    reason: reject_reason(a.reject_reason.as_deref()),
                });
                continue;
            }
            file_into(&mut files, a, failed_step, steps);
        }
        // Maestro's own media reads in the order its steps ran; the list's order is the upload's.
        files.own_media.sort_by_key(|m| {
            m.step
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        Self::Files(Box::new(files))
    }
}

/// Files one available artifact of the flow into `files`.
fn file_into(
    files: &mut EvidenceFiles,
    a: &ArtifactView,
    failed_step: Option<&StepKey>,
    steps: &[StepKey],
) {
    let class = ArtifactClass::parse(&a.class);
    let Some(link) = &a.link else { return };
    #[expect(
        clippy::cast_precision_loss,
        reason = "an artifact is far smaller than 2^52 bytes, and the size is only displayed"
    )]
    let size = Unit::Bytes.format(a.size_bytes.max(0) as f64);
    let open = || EvidenceLink {
        kind: kind(class),
        name: a.name.clone(),
        size: size.clone(),
        href: link.url.clone(),
    };
    let text = || TextFile {
        id: a.id,
        name: a.name.clone(),
        url: link.url.clone(),
    };
    match class {
        Some(ArtifactClass::FailureScreenshot) => files.screenshots.push(Shot {
            step: shot_step(&a.name, a.step, failed_step, steps),
            label: a.name.clone(),
            url: link.url.clone(),
        }),
        Some(ArtifactClass::FailureHierarchy) => {
            let step = shot_step(&a.name, a.step, failed_step, steps);
            files.hierarchies.push(Hierarchy {
                step_label: steps
                    .iter()
                    .find(|s| s.id == step)
                    .and_then(|s| s.label.clone()),
                failed: failed_step.is_some_and(|f| f.id == step),
                step,
                link: open(),
            });
        }
        Some(ArtifactClass::UserScreenshot | ArtifactClass::UserRecording) => {
            // Its step is the one `artifacts[]` named, and no other: a file no step names shows
            // none (I3), rather than the failed step it might not belong to.
            let step = a.step.map(|s| s.to_string());
            files.own_media.push(OwnMedia {
                id: a.id,
                kind: if class == Some(ArtifactClass::UserRecording) {
                    MediaKind::Recording
                } else {
                    MediaKind::Screenshot
                },
                step_label: step.as_ref().and_then(|id| {
                    steps
                        .iter()
                        .find(|s| &s.id == id)
                        .and_then(|s| s.label.clone())
                }),
                step,
                name: a.name.clone(),
                size: size.clone(),
                url: link.url.clone(),
            });
        }
        Some(ArtifactClass::HtmlReport) => files.report = Some(open()),
        Some(ArtifactClass::RunnerLog) => {
            files.log = Some(if a.size_bytes > MAX_LOG_BYTES {
                LogSource::TooLarge(open())
            } else {
                LogSource::Show(text())
            });
            files.downloads.push(open());
        }
        Some(ArtifactClass::Commands) => {
            files.commands = Some(text());
            files.downloads.push(open());
        }
        Some(ArtifactClass::DeviceLogcat) => {
            files.logcat = Some(if a.size_bytes > MAX_LOG_BYTES {
                LogSource::TooLarge(open())
            } else {
                LogSource::Show(text())
            });
            files.downloads.push(open());
        }
        Some(ArtifactClass::Junit) | None => {
            files.downloads.push(open());
        }
    }
}

/// When to ask for fresh links, in milliseconds from `now_ms`: a minute before the earliest link
/// of an available file lapses, but never sooner than a second (no tight loop). `None` when no
/// file has a link.
#[must_use]
pub fn renew_in_ms(list: &ArtifactListView, now_ms: i64) -> Option<i64> {
    list.artifacts
        .iter()
        .filter(|a| a.state == "available")
        .filter_map(|a| a.link.as_ref())
        .map(|l| unix_ms(l.expires_at))
        .min()
        .map(|earliest| (earliest - now_ms - RENEW_BEFORE_MS).max(1_000))
}

/// When to ask for fresh links, in milliseconds from `now_ms`, given the last refresh's outcome.
/// [`renew_in_ms`] normally. After a refresh that failed for a reason that may pass, the links
/// still held are tried again soon. A refusal that retrying cannot fix (signed out, forbidden)
/// is not retried. `None` when there is no link to renew.
#[must_use]
pub fn refresh_in_ms(
    list: Option<&ArtifactListView>,
    error: Option<&ApiError>,
    now_ms: i64,
) -> Option<i64> {
    let wait = renew_in_ms(list?, now_ms)?;
    match error {
        None => Some(wait),
        Some(ApiError::Network(_)) => Some(RETRY_AFTER_FAILURE_MS),
        Some(ApiError::Status { status, .. }) if *status >= 500 => Some(RETRY_AFTER_FAILURE_MS),
        Some(_) => None,
    }
}

/// What a class of file is called.
fn kind(class: Option<ArtifactClass>) -> &'static str {
    match class {
        Some(ArtifactClass::FailureScreenshot) => "Failure screenshot",
        Some(ArtifactClass::FailureHierarchy) => "UI hierarchy",
        Some(ArtifactClass::Commands) => "commands.json",
        Some(ArtifactClass::Junit) => "JUnit report",
        Some(ArtifactClass::RunnerLog) => "Maestro log",
        Some(ArtifactClass::HtmlReport) => "Maestro HTML report",
        Some(ArtifactClass::DeviceLogcat) => "Device logcat excerpt",
        Some(ArtifactClass::UserScreenshot) => "Maestro screenshot",
        Some(ArtifactClass::UserRecording) => "Maestro recording",
        None => "Other file",
    }
}

/// Why the server refused a file, in words; a code this build doesn't know is shown as sent.
fn reject_reason(code: Option<&str>) -> String {
    let Some(code) = code else {
        return "no reason was recorded".to_owned();
    };
    match RejectReason::parse(code) {
        Some(RejectReason::ClassNotAllowed) => "a kind of file Fleet does not accept".to_owned(),
        Some(RejectReason::ContentTypeNotAllowed) => {
            "a content type this kind of file cannot have".to_owned()
        }
        Some(RejectReason::TooLarge) => {
            "bigger than the size limit for this kind of file".to_owned()
        }
        Some(RejectReason::RunQuotaExceeded) => {
            "the run had already uploaded as much as one run may".to_owned()
        }
        Some(RejectReason::ProjectQuotaExceeded) => {
            "the project had already uploaded as much as one day allows".to_owned()
        }
        Some(RejectReason::SizeMismatch) => "the bytes were not the declared size".to_owned(),
        Some(RejectReason::DigestMismatch) => {
            "the bytes did not match the declared SHA-256".to_owned()
        }
        Some(RejectReason::TypeNotAllowed) => {
            "the content is not a type this kind of file may have".to_owned()
        }
        Some(RejectReason::NotText) => "a text file that is not UTF-8 text".to_owned(),
        Some(RejectReason::UnsafeArchive) => {
            "an archive that is unsafe to open (a bomb, a path escape, a link or a nested archive)"
                .to_owned()
        }
        None => code.to_owned(),
    }
}

/// The step a screenshot belongs to. The step the agent linked from Maestro's `artifacts[]` is
/// exact, so it wins. The file name is the fallback only for an artifact without one: uploads
/// that predate stored step links, and output without `artifacts[]`. Those name the file
/// `step-<n>-…` with `n` counting from one, the step whose index is `n - 1`. Otherwise the failed
/// step; otherwise none (an empty id).
fn shot_step(name: &str, step: Option<u32>, failed: Option<&StepKey>, steps: &[StepKey]) -> String {
    if let Some(step) = step {
        return step.to_string();
    }
    let named = name
        .strip_prefix("step-")
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(n, _)| n.parse::<u64>().ok())
        .and_then(|n| n.checked_sub(1))
        .map(|n| n.to_string())
        .filter(|id| steps.iter().any(|s| &s.id == id));
    named
        .or_else(|| failed.map(|s| s.id.clone()))
        .unwrap_or_default()
}

/// The redacted Maestro log, ready for the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvidence {
    /// The text, as lines.
    pub model: LogModel,
    /// Where each step of this flow begins in the text.
    pub starts: Vec<StepStart>,
}

impl LogEvidence {
    /// Parses `text`, the session log, and finds where `flow_name`'s steps start.
    #[must_use]
    pub fn build(text: &str, flow_name: &str, steps: &[StepKey]) -> Self {
        Self {
            model: LogModel::parse(text),
            starts: step_starts(text, flow_name, steps),
        }
    }

    /// The zero-based line where step `id` starts.
    #[must_use]
    pub fn line_of(&self, id: &str) -> Option<usize> {
        self.model.jump_to_step(&self.starts, id)
    }
}

/// The message of a Maestro session-log record (`MFLT1|<ms>|<thread>|<level>|<logger>|<message>`)
/// with a leading `[shard N] ` removed; `None` for any other line (a continuation, another format).
fn record_message(line: &str) -> Option<&str> {
    let mut fields = line.splitn(6, '|');
    (fields.next()? == "MFLT1").then_some(())?;
    let msg = fields.nth(4)?;
    Some(
        msg.strip_prefix("[shard ")
            .and_then(|r| r.split_once("] "))
            .map_or(msg, |(_, rest)| rest),
    )
}

/// Where each of `steps` starts in `text`, looking only at the part of the log that belongs to
/// `flow_name` (a suite's log holds every flow, and steps like "Launch app" repeat). A step starts
/// at the `<description> RUNNING` line; steps are matched in order, so a step the log doesn't show
/// is skipped without losing the ones after it.
fn step_starts(text: &str, flow_name: &str, steps: &[StepKey]) -> Vec<StepStart> {
    let mut starts = Vec::new();
    let mut in_flow = false;
    let mut next = 0;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line_start = offset;
        offset += raw.len();
        let Some(msg) = record_message(raw.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        if let Some(name) = msg.trim_start().strip_prefix("Running flow ") {
            in_flow = name.trim() == flow_name;
            next = 0;
            continue;
        }
        let Some(description) = in_flow.then(|| msg.strip_suffix(" RUNNING")).flatten() else {
            continue;
        };
        let remaining = steps.get(next..).unwrap_or_default();
        if let Some(found) = remaining
            .iter()
            .position(|s| s.label.as_deref() == Some(description))
        {
            if let Some(step) = remaining.get(found) {
                starts.push(StepStart {
                    step: step.id.clone(),
                    offset: line_start,
                });
            }
            next += found + 1;
        }
    }
    starts
}

/// One command of `commands.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRow {
    /// Its order in the flow.
    pub sequence: u64,
    /// How deeply it is nested (`runFlow`, repeats).
    pub depth: u64,
    /// What it is, in words.
    pub label: String,
    /// How it ended, in words.
    pub status: &'static str,
    /// The icon and colour of the word.
    pub mark: Mark,
    /// How long it took.
    pub duration: Option<String>,
}

/// Reads `commands.json` (Maestro's per-command metadata) into rows in order. `Err` says what is
/// wrong with a file that is not that.
pub fn command_rows(text: &str) -> Result<Vec<CommandRow>, &'static str> {
    let value: Value = serde_json::from_str(text).map_err(|_| "commands.json is not valid JSON")?;
    let entries = value
        .as_array()
        .ok_or("commands.json is not a list of commands")?;
    let mut rows: Vec<CommandRow> = entries.iter().filter_map(command_row).collect();
    rows.sort_by_key(|r| r.sequence);
    Ok(rows)
}

fn command_row(entry: &Value) -> Option<CommandRow> {
    let name = entry.get("command")?.as_object()?.keys().next()?;
    let meta = entry.get("metadata")?;
    let (status, mark) = match meta.get("status").and_then(Value::as_str) {
        Some("COMPLETED") => ("Passed", Mark::Passed),
        Some("FAILED") => ("Failed", Mark::Failed),
        Some("SKIPPED") => ("Skipped", Mark::Skipped),
        Some("WARNED") => ("Warned", Mark::Interrupted),
        Some("RUNNING") => ("Running", Mark::Running),
        Some("PENDING") => ("Not started", Mark::NotStarted),
        _ => ("Unknown", Mark::Unknown),
    };
    Some(CommandRow {
        sequence: meta.get("sequenceNumber")?.as_u64()?,
        depth: meta.get("depth").and_then(Value::as_u64).unwrap_or(0),
        label: words(name),
        status,
        mark,
        duration: meta
            .get("duration")
            .and_then(Value::as_i64)
            .map(format_duration),
    })
}

/// `assertConditionCommand` as words: `Assert condition`.
fn words(command: &str) -> String {
    let name = command.strip_suffix("Command").unwrap_or(command);
    let mut out = String::new();
    for c in name.chars() {
        if c.is_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        if out.is_empty() {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: Uuid = Uuid::from_u128(0xf2);
    const NOW: i64 = 1_767_265_200_000; // 2026-01-01T11:00:00Z

    fn step(id: &str, label: &str) -> StepKey {
        StepKey {
            id: id.to_owned(),
            label: Some(label.to_owned()),
        }
    }

    fn artifact(
        id: u128,
        class: &str,
        name: &str,
        ct: &str,
        size: i64,
        url: Option<&str>,
    ) -> Value {
        serde_json::json!({
            "id": Uuid::from_u128(id), "flow_execution_id": null, "class": class, "name": name,
            "path": format!("run/{name}"), "content_type": ct, "size_bytes": size,
            "sha256": "0".repeat(64), "state": "available", "reject_reason": null,
            "retention_class": "failure_evidence", "created_at": "2026-01-01T10:20:00Z",
            "link": url.map(|u| serde_json::json!({"url": u, "expires_at": "2026-01-01T11:05:00Z"})),
        })
    }

    fn list(artifacts: &[Value], usercontent: Option<&str>) -> ArtifactListView {
        serde_json::from_value(serde_json::json!({
            "artifacts": artifacts, "usercontent_url": usercontent,
        }))
        .unwrap()
    }

    const UC: &str = "https://uc.example.net";

    fn full() -> ArtifactListView {
        let mut shot = artifact(
            1,
            "failure_screenshot",
            "step-003-assertCondition-Done.png",
            "image/png",
            52_000,
            Some("https://uc.example.net/u/p/1/1/s/shot.png"),
        );
        shot["flow_execution_id"] = serde_json::json!(FLOW);
        list(
            &[
                shot,
                artifact(
                    2,
                    "commands",
                    "commands.json",
                    "application/json",
                    4_255,
                    Some("https://uc.example.net/u/p/2/1/s/commands.json"),
                ),
                artifact(
                    3,
                    "runner_log",
                    "maestro.log",
                    "text/plain",
                    1_703,
                    Some("https://uc.example.net/u/p/3/1/s/maestro.log"),
                ),
                artifact(
                    4,
                    "junit",
                    "report.xml",
                    "application/xml",
                    900,
                    Some("https://uc.example.net/u/p/4/1/s/report.xml"),
                ),
                artifact(
                    5,
                    "html_report",
                    "index.html",
                    "text/html",
                    2_048,
                    Some("https://uc.example.net/u/p/5/1/s/index.html"),
                ),
            ],
            Some(UC),
        )
    }

    fn steps() -> Vec<StepKey> {
        vec![
            step("0", "Launch app \"x\""),
            step("1", "Tap on \"Pay\""),
            step("2", "Assert that \"Done\" is visible"),
        ]
    }

    fn files(e: Evidence) -> EvidenceFiles {
        match e {
            Evidence::Files(f) => *f,
            other => panic!("expected files, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_flows_files_are_sorted_into_viewer_log_timings_report_and_downloads() {
        let steps = steps();
        let f = files(Evidence::build(&full(), FLOW, steps.last(), &steps));

        // The file name `step-003-…` says step 3 (1-based) took it: the step with index 2.
        assert_eq!(
            f.screenshots,
            [Shot {
                step: "2".to_owned(),
                label: "step-003-assertCondition-Done.png".to_owned(),
                url: "https://uc.example.net/u/p/1/1/s/shot.png".to_owned(),
            }]
        );
        assert_eq!(
            f.log,
            Some(LogSource::Show(TextFile {
                id: Uuid::from_u128(3),
                name: "maestro.log".to_owned(),
                url: "https://uc.example.net/u/p/3/1/s/maestro.log".to_owned(),
            }))
        );
        assert_eq!(
            f.commands.map(|c| (c.id, c.name)),
            Some((Uuid::from_u128(2), "commands.json".to_owned()))
        );
        assert_eq!(
            f.report,
            Some(EvidenceLink {
                kind: "Maestro HTML report",
                name: "index.html".to_owned(),
                size: "2 KiB".to_owned(),
                href: "https://uc.example.net/u/p/5/1/s/index.html".to_owned(),
            })
        );
        let downloads: Vec<_> = f
            .downloads
            .iter()
            .map(|d| (d.kind, d.name.as_str(), d.size.as_str()))
            .collect();
        assert_eq!(
            downloads,
            [
                ("commands.json", "commands.json", "4.16 KiB"),
                ("Maestro log", "maestro.log", "1.66 KiB"),
                ("JUnit report", "report.xml", "900 B"),
            ]
        );
        assert_eq!(f.refused, Vec::new());
    }

    /// The failing step's UI hierarchy is linked from that step, in its own list, not among the
    /// downloads; an earlier step's hierarchy names that step instead.
    #[test]
    fn a_ui_hierarchy_is_linked_from_the_step_it_was_taken_at() {
        let steps = steps();
        let mut failed = artifact(
            9,
            "failure_hierarchy",
            "step-003-assertCondition-Done.json",
            "application/json",
            1_200,
            Some("https://uc.example.net/u/p/9/1/s/hierarchy.json"),
        );
        failed["flow_execution_id"] = serde_json::json!(FLOW);
        let mut earlier = artifact(
            10,
            "failure_hierarchy",
            "step-002-tapOn-Menu.json",
            "application/json",
            800,
            Some("https://uc.example.net/u/p/10/1/s/earlier.json"),
        );
        earlier["flow_execution_id"] = serde_json::json!(FLOW);
        let f = files(Evidence::build(
            &list(&[failed, earlier], Some(UC)),
            FLOW,
            steps.last(),
            &steps,
        ));

        let link = |href: &str, name: &str| EvidenceLink {
            kind: "UI hierarchy",
            name: name.to_owned(),
            size: if name == "step-003-assertCondition-Done.json" {
                "1.17 KiB".to_owned()
            } else {
                "800 B".to_owned()
            },
            href: href.to_owned(),
        };
        assert_eq!(
            f.hierarchies,
            [
                Hierarchy {
                    step: "2".to_owned(),
                    step_label: Some("Assert that \"Done\" is visible".to_owned()),
                    failed: true,
                    link: link(
                        "https://uc.example.net/u/p/9/1/s/hierarchy.json",
                        "step-003-assertCondition-Done.json"
                    ),
                },
                Hierarchy {
                    step: "1".to_owned(),
                    step_label: Some(steps[1].label.clone().unwrap_or_default()),
                    failed: false,
                    link: link(
                        "https://uc.example.net/u/p/10/1/s/earlier.json",
                        "step-002-tapOn-Menu.json"
                    ),
                },
            ]
        );
        assert!(f.downloads.is_empty(), "{:?}", f.downloads);
    }

    /// The device logcat excerpt of a failed flow is offered as a download with its own label
    /// Maestro's own screenshot and recording of the flow, named the steps that made them, plus a
    /// screenshot no step names (issue #133).
    fn own_media_files() -> EvidenceFiles {
        let mut shot = artifact(
            7,
            "user_screenshot",
            "fleet-before-fail.png",
            "image/png",
            2_048,
            Some("https://uc.example.net/u/p/7/1/s/fleet-before-fail.png"),
        );
        shot["flow_execution_id"] = serde_json::json!(FLOW);
        shot["step"] = serde_json::json!(4);
        let mut rec = artifact(
            8,
            "user_recording",
            "fleet-fail-recording.mp4",
            "video/mp4",
            4_096,
            Some("https://uc.example.net/u/p/8/1/s/fleet-fail-recording.mp4"),
        );
        rec["flow_execution_id"] = serde_json::json!(FLOW);
        rec["step"] = serde_json::json!(5);
        let mut loose = artifact(
            9,
            "user_screenshot",
            "loose.png",
            "image/png",
            2_048,
            Some("https://uc.example.net/u/p/9/1/s/loose.png"),
        );
        loose["flow_execution_id"] = serde_json::json!(FLOW);
        let steps = vec![
            step("4", "Take screenshot fleet-before-fail"),
            step("5", "Start recording fleet-fail-recording"),
            step("8", "Assert condition"),
        ];
        // The recording is listed first: the page reads the media in step order anyway.
        files(Evidence::build(
            &list(&[rec, shot, loose], Some(UC)),
            FLOW,
            steps.last(),
            &steps,
        ))
    }

    /// A screenshot or recording the flow made on purpose is shown on the step that made it, as its
    /// own kind (issue #133): an image for the screenshot, a video for the recording. It is not a
    /// failure screenshot, and no step is guessed for a file no step names.
    #[test]
    fn a_users_screenshot_and_recording_are_shown_on_the_step_that_made_them() {
        let f = own_media_files();
        assert_eq!(
            f.own_media[..2],
            [
                OwnMedia {
                    id: Uuid::from_u128(7),
                    kind: MediaKind::Screenshot,
                    step: Some("4".to_owned()),
                    step_label: Some("Take screenshot fleet-before-fail".to_owned()),
                    name: "fleet-before-fail.png".to_owned(),
                    size: "2 KiB".to_owned(),
                    url: "https://uc.example.net/u/p/7/1/s/fleet-before-fail.png".to_owned(),
                },
                OwnMedia {
                    id: Uuid::from_u128(8),
                    kind: MediaKind::Recording,
                    step: Some("5".to_owned()),
                    step_label: Some("Start recording fleet-fail-recording".to_owned()),
                    name: "fleet-fail-recording.mp4".to_owned(),
                    size: "4 KiB".to_owned(),
                    url: "https://uc.example.net/u/p/8/1/s/fleet-fail-recording.mp4".to_owned(),
                },
            ]
        );
        assert_eq!(
            f.own_media[2].step, None,
            "no step names it, so none is shown"
        );
        assert_eq!(f.own_media[2].step_label, None);
        assert!(f.screenshots.is_empty(), "not a failure screenshot");
        assert!(
            f.downloads.is_empty(),
            "shown inline, not offered as a download"
        );
        assert!(
            !f.mounts().screenshots,
            "the failure screenshots' viewer is unaffected by Maestro's own media"
        );
    }

    /// A refused recording is listed with its reason and no player (issue #133).
    #[test]
    fn a_refused_recording_is_listed_with_its_reason_and_no_player() {
        let mut rec = artifact(
            10,
            "user_recording",
            "fleet-rec.mp4",
            "video/mp4",
            4_096,
            None,
        );
        rec["flow_execution_id"] = serde_json::json!(FLOW);
        rec["state"] = serde_json::json!("rejected");
        rec["reject_reason"] = serde_json::json!("type_not_allowed");
        let steps = steps();
        let f = files(Evidence::build(
            &list(&[rec], Some(UC)),
            FLOW,
            steps.last(),
            &steps,
        ));
        assert_eq!(f.own_media, Vec::<OwnMedia>::new());
        assert_eq!(
            f.refused,
            [Refused {
                name: "fleet-rec.mp4".to_owned(),
                kind: "Maestro recording",
                reason: "the content is not a type this kind of file may have".to_owned(),
            }]
        );
    }

    /// (issue #75): the link opens the excerpt, and the failure's step tree is unchanged.
    #[test]
    fn a_device_logcat_excerpt_is_a_download_labelled_as_such() {
        let mut excerpt = artifact(
            6,
            "device_logcat",
            "device-logcat.txt",
            "text/plain",
            2_048,
            Some("https://uc.example.net/u/p/6/1/s/device-logcat.txt"),
        );
        excerpt["flow_execution_id"] = serde_json::json!(FLOW);
        let steps = steps();
        let f = files(Evidence::build(
            &list(&[excerpt], Some(UC)),
            FLOW,
            steps.last(),
            &steps,
        ));
        assert_eq!(
            f.downloads,
            [EvidenceLink {
                kind: "Device logcat excerpt",
                name: "device-logcat.txt".to_owned(),
                size: "2 KiB".to_owned(),
                href: "https://uc.example.net/u/p/6/1/s/device-logcat.txt".to_owned(),
            }]
        );
        assert!(f.log.is_none(), "the excerpt is not the Maestro log");
    }

    #[test]
    fn a_screenshot_with_no_step_in_its_name_belongs_to_the_failed_step() {
        let mut l = full();
        l.artifacts[0].name = "failure.png".to_owned();
        let steps = steps();
        let f = files(Evidence::build(&l, FLOW, steps.last(), &steps));
        assert_eq!(f.screenshots[0].step, "2");
        // With no failed step either, it is not tied to any step.
        let f = files(Evidence::build(&l, FLOW, None, &steps));
        assert_eq!(f.screenshots[0].step, "");
    }

    /// The agent's `artifacts[]` link beats the file name: the name says step 3 (index 2), the
    /// step Maestro named is index 1.
    #[test]
    fn a_screenshot_with_a_step_takes_that_step_over_its_file_name() {
        let mut shot = artifact(
            1,
            "failure_screenshot",
            "step-003-assertCondition-Done.png",
            "image/png",
            52_000,
            Some("https://uc.example.net/u/p/1/1/s/shot.png"),
        );
        shot["flow_execution_id"] = serde_json::json!(FLOW);
        shot["step"] = serde_json::json!(1);
        let steps = steps();
        let f = files(Evidence::build(
            &list(&[shot], Some(UC)),
            FLOW,
            steps.last(),
            &steps,
        ));
        assert_eq!(f.screenshots[0].step, "1");
    }

    #[test]
    fn another_flows_screenshot_is_not_shown() {
        let mut l = full();
        l.artifacts[0].flow_execution_id = Some(Uuid::from_u128(0xf3));
        let steps = steps();
        let f = files(Evidence::build(&l, FLOW, None, &steps));
        assert_eq!(f.screenshots, Vec::new());
    }

    /// Renewing the links changes every URL but none of what the card mounts, so the viewers
    /// stay mounted across renewals; a file that goes away is a change.
    #[test]
    fn renewed_links_leave_what_the_card_mounts_unchanged() {
        let steps = steps();
        let before = files(Evidence::build(&full(), FLOW, steps.last(), &steps));
        let mut renewed = full();
        for a in &mut renewed.artifacts {
            if let Some(link) = a.link.as_mut() {
                link.url.push_str("?renewed");
            }
        }
        let after = files(Evidence::build(&renewed, FLOW, steps.last(), &steps));
        assert_ne!(before.screenshots, after.screenshots, "the links changed");
        assert_eq!(before.mounts(), after.mounts());

        renewed.artifacts.retain(|a| a.name != "commands.json");
        let gone = files(Evidence::build(&renewed, FLOW, steps.last(), &steps));
        assert_ne!(before.mounts(), gone.mounts());
    }

    #[test]
    fn nothing_uploaded_is_said_plainly() {
        let e = Evidence::build(&list(&[], Some(UC)), FLOW, None, &[]);
        let Evidence::Nothing(text) = e else {
            panic!("expected the empty state, got {e:?}");
        };
        assert!(
            text.starts_with("No evidence was uploaded for this flow"),
            "{text}"
        );
        assert!(text.contains("fork pull request"), "{text}");
    }

    #[test]
    fn files_the_server_cannot_open_are_stored_not_lost() {
        let l = list(
            &[artifact(
                3,
                "runner_log",
                "maestro.log",
                "text/plain",
                10,
                None,
            )],
            None,
        );
        let e = Evidence::build(&l, FLOW, None, &[]);
        let Evidence::Unopenable(text) = e else {
            panic!("expected the unopenable state, got {e:?}");
        };
        assert!(text.contains("no usercontent origin"), "{text}");
    }

    #[test]
    fn a_refused_upload_is_listed_with_its_reason_and_has_no_link() {
        let mut bad = artifact(6, "html_report", "report.zip", "application/zip", 9, None);
        bad["state"] = "rejected".into();
        bad["reject_reason"] = "unsafe_archive".into();
        let mut odd = artifact(7, "junit", "r.xml", "application/xml", 9, None);
        odd["state"] = "rejected".into();
        odd["reject_reason"] = "from_the_future".into();
        let l = list(&[bad, odd], Some(UC));
        let f = files(Evidence::build(&l, FLOW, None, &[]));
        assert_eq!(
            f.refused,
            [
                Refused {
                    name: "report.zip".to_owned(),
                    kind: "Maestro HTML report",
                    reason: "an archive that is unsafe to open (a bomb, a path escape, a link or a nested archive)".to_owned(),
                },
                Refused {
                    name: "r.xml".to_owned(),
                    kind: "JUnit report",
                    reason: "from_the_future".to_owned(),
                },
            ]
        );
        assert!(f.report.is_none() && f.downloads.is_empty());
    }

    #[test]
    fn a_log_too_big_for_the_browser_is_a_download_not_a_fetch() {
        let mut l = full();
        l.artifacts[2].size_bytes = MAX_LOG_BYTES + 1;
        let f = files(Evidence::build(&l, FLOW, None, &[]));
        let Some(LogSource::TooLarge(link)) = f.log else {
            panic!("expected a download, got {:?}", f.log);
        };
        assert_eq!((link.kind, link.size.as_str()), ("Maestro log", "8 MiB"));
        // Exactly the limit is still shown.
        l.artifacts[2].size_bytes = MAX_LOG_BYTES;
        let f = files(Evidence::build(&l, FLOW, None, &[]));
        assert!(matches!(f.log, Some(LogSource::Show(_))));
    }

    #[test]
    fn links_are_renewed_a_minute_before_the_earliest_lapses() {
        // Expiry 11:05:00, now 11:00:00: 300 s − 60 s.
        assert_eq!(renew_in_ms(&full(), NOW), Some(240_000));
        // Already inside the last minute: soon, but never a tight loop.
        assert_eq!(renew_in_ms(&full(), NOW + 280_000), Some(1_000));
        // The earliest of several decides.
        let mut l = full();
        l.artifacts[1].link.as_mut().unwrap().expires_at =
            time::OffsetDateTime::from_unix_timestamp(1_767_265_320).unwrap(); // 11:02:00Z
        assert_eq!(renew_in_ms(&l, NOW), Some(60_000));
        // Nothing to open, nothing to renew.
        assert_eq!(renew_in_ms(&list(&[], Some(UC)), NOW), None);
    }

    #[test]
    fn a_failed_refresh_is_tried_again_soon_while_its_links_are_held() {
        let l = full();
        let offline = ApiError::Network("offline".to_owned());
        assert_eq!(refresh_in_ms(Some(&l), None, NOW), Some(240_000));
        // A renewal that failed (the laptop woke before the network) must not leave the expired
        // links on screen until a reload.
        assert_eq!(
            refresh_in_ms(Some(&l), Some(&offline), NOW),
            Some(RETRY_AFTER_FAILURE_MS)
        );
        let busy = ApiError::Status {
            status: 503,
            message: "busy".to_owned(),
        };
        assert_eq!(
            refresh_in_ms(Some(&l), Some(&busy), NOW),
            Some(RETRY_AFTER_FAILURE_MS)
        );
        // Retrying cannot fix a lapsed sign-in, a refusal or a bad answer: stop, no loop.
        assert_eq!(
            refresh_in_ms(Some(&l), Some(&ApiError::Unauthorized), NOW),
            None
        );
        assert_eq!(
            refresh_in_ms(
                Some(&l),
                Some(&ApiError::Status {
                    status: 400,
                    message: "bad".to_owned(),
                }),
                NOW
            ),
            None
        );
        // Nothing to renew, failed or not.
        assert_eq!(refresh_in_ms(None, Some(&offline), NOW), None);
        assert_eq!(
            refresh_in_ms(Some(&list(&[], Some(UC))), Some(&offline), NOW),
            None
        );
    }

    // The real session log of the captured suite run, trimmed to two flows (fixtures/maestro/
    // 2.12.1/suite). Line numbers are zero-based in the comments.
    const LOG: &str = "\
MFLT1|1793751906337|main|INFO|maestro.cli.report.TestDebugReporter|Debug output path: <CAPTURE>/run/debug\n\
MFLT1|1793751910306|w-3|INFO|maestro.cli.runner.TestSuiteInteractor| Running flow alpha\n\
MFLT1|1793751910648|w-3|INFO|maestro.cli.runner.CliConsoleListener|Launch app \"com.android.settings\" RUNNING\n\
MFLT1|1793751910896|w-3|INFO|maestro.cli.runner.CliConsoleListener|Launch app \"com.android.settings\" COMPLETED\n\
MFLT1|1793751920306|w-3|INFO|maestro.cli.runner.TestSuiteInteractor| Running flow delta-fails\n\
MFLT1|1793751920648|w-3|INFO|maestro.cli.runner.CliConsoleListener|Launch app \"com.android.settings\" RUNNING\n\
MFLT1|1793751920896|w-3|INFO|maestro.cli.runner.CliConsoleListener|Launch app \"com.android.settings\" COMPLETED\n\
MFLT1|1793751920897|w-3|INFO|maestro.cli.runner.CliConsoleListener|Assert that \"Fleet never shows this\" is visible RUNNING\n\
MFLT1|1793751928077|w-3|ERROR|maestro.orchestra.Orchestra|[Command execution] CommandFailed: Assertion is false\n\
MFLT1|1793751928543|w-3|INFO|maestro.cli.runner.CliConsoleListener|Assert that \"Fleet never shows this\" is visible FAILED\n";

    fn delta_steps() -> Vec<StepKey> {
        vec![
            step("0", "Launch app \"com.android.settings\""),
            step("1", "Assert that \"Fleet never shows this\" is visible"),
        ]
    }

    #[test]
    fn each_step_starts_at_its_running_line_inside_its_own_flow() {
        let log = LogEvidence::build(LOG, "delta-fails", &delta_steps());
        assert_eq!(log.model.lines().len(), 10);
        // Not line 2: alpha ran the same "Launch app" step first.
        assert_eq!(log.line_of("0"), Some(5));
        assert_eq!(log.line_of("1"), Some(7));
        assert_eq!(log.line_of("9"), None, "no such step");
    }

    #[test]
    fn a_shard_prefix_is_ignored_and_a_flow_that_is_not_in_the_log_gets_no_jump() {
        let sharded = LOG.replace("|Launch app", "|[shard 1] Launch app");
        let log = LogEvidence::build(&sharded, "delta-fails", &delta_steps());
        assert_eq!(log.line_of("0"), Some(5));
        let log = LogEvidence::build(LOG, "no-such-flow", &delta_steps());
        assert_eq!(log.line_of("0"), None);
        assert_eq!(log.starts, Vec::new());
        assert_eq!(log.model.lines().len(), 10, "the log is still readable");
    }

    #[test]
    fn a_log_that_is_not_maestros_session_format_has_no_jumps_but_is_shown() {
        let log = LogEvidence::build("hello\nworld\n", "delta-fails", &delta_steps());
        assert_eq!(log.model.lines().len(), 2);
        assert_eq!(log.line_of("0"), None);
    }

    #[test]
    fn a_step_without_a_description_is_skipped_and_the_search_continues_after_the_last_hit() {
        let steps = vec![
            StepKey {
                id: "0".to_owned(),
                label: None,
            },
            step("1", "Assert that \"Fleet never shows this\" is visible"),
        ];
        let log = LogEvidence::build(LOG, "delta-fails", &steps);
        assert_eq!(log.line_of("0"), None);
        assert_eq!(log.line_of("1"), Some(7));
    }

    /// The captured suite run's own log (fixtures/maestro/2.12.1/suite): four flows, the third
    /// failing. The steps are the ones the server derives from that log for `delta-fails`.
    #[test]
    fn the_failing_step_of_the_captured_suite_run_is_found_in_its_real_log() {
        let text =
            include_str!("../../../../../fixtures/maestro/2.12.1/suite/run/debug/maestro.log");
        let steps = [
            step("0", "Define variables"),
            step("1", "Apply configuration"),
            step("2", "Launch app \"com.android.settings\""),
            step("3", "Assert that \"Fleet never shows this\" is visible"),
        ];
        let log = LogEvidence::build(text, "delta-fails", &steps);
        // `grep -n` of the fixture: the four RUNNING lines of delta-fails are lines 59, 61, 63
        // and 67 (1-based); bravo, charlie and alpha ran the first three steps too.
        let found: Vec<_> = ["0", "1", "2", "3"].map(|id| log.line_of(id)).to_vec();
        assert_eq!(found, [Some(58), Some(60), Some(62), Some(66)]);
        let line = log.model.line(66).unwrap();
        assert!(
            line.plain
                .ends_with("Assert that \"Fleet never shows this\" is visible RUNNING"),
            "{}",
            line.plain
        );
    }

    // The first commands of the captured `fail` run (fixtures/maestro/2.12.1/fail/.../commands.json).
    const COMMANDS: &str = r#"[
      {"command": {"defineVariablesCommand": {"optional": false}},
       "metadata": {"status": "COMPLETED", "timestamp": 1793751910644, "duration": 4,
                    "sequenceNumber": 0, "depth": 0}},
      {"command": {"launchAppCommand": {"appId": "com.android.settings"}},
       "metadata": {"status": "COMPLETED", "timestamp": 1793751910648, "duration": 248,
                    "sequenceNumber": 2, "depth": 0}},
      {"command": {"assertConditionCommand": {"condition": {}}},
       "metadata": {"status": "FAILED", "timestamp": 1793751912442, "duration": 15635,
                    "sequenceNumber": 4, "depth": 1,
                    "error": {"message": "Assertion is false"}}},
      {"command": {"tapOnElement": {}},
       "metadata": {"status": "SKIPPED", "sequenceNumber": 3, "depth": 0}}
    ]"#;

    #[test]
    fn commands_become_rows_in_order_with_words_for_name_status_and_duration() {
        let rows = command_rows(COMMANDS).unwrap();
        let shown: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.sequence,
                    r.depth,
                    r.label.as_str(),
                    r.status,
                    r.mark,
                    r.duration.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    0,
                    0,
                    "Define variables",
                    "Passed",
                    Mark::Passed,
                    Some("4 ms")
                ),
                (2, 0, "Launch app", "Passed", Mark::Passed, Some("248 ms")),
                (3, 0, "Tap on element", "Skipped", Mark::Skipped, None),
                (
                    4,
                    1,
                    "Assert condition",
                    "Failed",
                    Mark::Failed,
                    Some("15 s")
                ),
            ]
        );
    }

    #[test]
    fn a_status_this_build_does_not_know_is_shown_as_reported_never_as_a_success() {
        let json = r#"[{"command": {"waitCommand": {}},
            "metadata": {"status": "TELEPORTED", "sequenceNumber": 0, "depth": 0, "duration": 5}}]"#;
        let row = command_rows(json).unwrap().remove(0);
        assert_eq!((row.status, row.mark), ("Unknown", Mark::Unknown));
    }

    #[test]
    fn a_file_that_is_not_the_commands_array_is_an_error_not_a_panic() {
        assert_eq!(
            command_rows("not json"),
            Err("commands.json is not valid JSON")
        );
        assert_eq!(
            command_rows(r#"{"a": 1}"#),
            Err("commands.json is not a list of commands")
        );
        // Entries that are not commands are left out; the rest still show.
        let rows = command_rows(r#"[1, {"command": {}}, {"command": {"waitCommand": {}}, "metadata": {"sequenceNumber": 0}}]"#).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Wait");
        assert_eq!(rows[0].duration, None);
    }
}
