// SPDX-License-Identifier: AGPL-3.0-only
//! The session state machine: the one place observations become journal events (research 02
//! §15.2, "state actor"). The live observer and `fleet-agent inspect` both drive it, so offline
//! inspection exercises exactly the live code path.
//!
//! Every outcome is a claim with a provenance; [`fleet_core::claims::resolve`] decides what a
//! flow's outcome is (invariants I3, I4). Every text that leaves goes through the redactor (I6).

use fleet_core::{
    claims::{self, Claim, Resolution},
    reconcile::{self, FlowKey},
    redact::Redactor,
};
use fleet_maestro::{ReportState, bundle, crash, junit, log, logcat, stdout};
use fleet_protocol::{
    artifact::ArtifactClass,
    event::{
        AgentWarning, AppCrashed, ArtifactRegistered, EventBody, EvidenceSources, FailureInfo,
        FlowEnded, FlowOutcome, FlowRef, FlowReported, FlowStarted, MaestroBuild, ProcessExited,
        ReportParsed, ReportStatus, RunEnded, RunSignal, RunStarted, ShardAssigned, ShardMode,
        StepEnded, StepStarted, StepStatus, SuiteStarted,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, StepRef, Subject},
};
use uuid::Uuid;

/// Longest failure message kept, in characters.
const MAX_MESSAGE: usize = 2000;

/// Something the agent saw.
#[derive(Debug, Clone)]
pub(crate) enum Observation {
    /// A complete stdout line.
    Stdout(String),
    /// A complete record of the session `maestro.log`.
    Log(log::Record),
    /// A file appeared or changed under the debug output directory (relative path) — with the
    /// bytes of `commands.json` when that's the file.
    File {
        rel: String,
        contents: Option<Vec<u8>>,
    },
    /// The supervisor forwarded a signal.
    Signal(i32),
    /// Maestro exited.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        /// Whether a report was expected and written.
        report: ReportState,
    },
    /// The final report, or `None` when there is none.
    Report(Option<Vec<u8>>),
    /// This run executes a shard of a Fleet plan.
    Plan(fleet_protocol::event::PlanAssigned),
    /// adb's current device list.
    Devices(Vec<(String, fleet_protocol::event::AdbState)>),
}

/// A flow execution as the session tracks it.
#[derive(Debug, Clone)]
pub(crate) struct Flow {
    pub(crate) id: Uuid,
    pub(crate) shard: u32,
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    pub(crate) ordinal: u32,
    pub(crate) started: bool,
    pub(crate) claims: Vec<Claim>,
    pub(crate) steps: u32,
    /// Steps Maestro has started and not yet ended, innermost last.
    open_steps: Vec<OpenStep>,
    pub(crate) failure: Option<String>,
    /// The flow's `manifest.json` appeared: Maestro finished writing the bundle.
    manifest_seen: bool,
    /// A fully terminal, failure-free `commands.json` waiting for the manifest before it may
    /// claim `passed` (I3).
    pending_pass: Option<String>,
    /// Which step produced each of the flow's files, once its `commands.json` has commands.
    produced: Produced,
    /// Step-bound files seen before `produced` was known: their step waits for it.
    pending_files: Vec<PendingFile>,
}

/// A step Maestro started and has not ended (issue #38: the nesting and retry evidence).
#[derive(Debug, Clone)]
struct OpenStep {
    index: u32,
    description: String,
    /// Set on a `retry` block: the attempt that runs now.
    retry: Option<Retry>,
}

/// The attempts of a `retry` block, as Maestro's `Attempt N failed for retry command` lines count
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Attempt `n` is running.
    Attempt(u32),
    /// The log stopped agreeing with the count, so the attempts are unknown (I3: none is guessed).
    Unknown,
}

/// Which step produced a flow's files, as its `commands.json` says.
#[derive(Debug, Clone, Default)]
enum Produced {
    /// No `commands.json` with commands yet: a file's step waits for it.
    #[default]
    Unknown,
    /// `artifacts[]` is there: flow-relative path to the producing command's sequence number.
    Listed(std::collections::BTreeMap<String, u32>),
    /// Older output without `artifacts[]`: the `step-NNN-…` file name is the only link.
    FromFileNames,
}

impl Produced {
    /// The step a file of the flow belongs to (`named` is the step its file name gives, 0-based).
    /// A file `artifacts[]` does not list has none. Not called while [`Produced::Unknown`].
    ///
    /// A step index is the command's sequence number: Maestro numbers commands in the order it
    /// starts them and logs one step per command, so the two match (the inspect tests check this on
    /// the captured fixtures).
    fn step_of(&self, rel: &str, named: Option<u32>) -> Option<u32> {
        // `rel` starts with the flow folder; `artifacts[]` paths are relative to it.
        let in_flow = rel.split_once('/').map_or(rel, |(_, rest)| rest);
        match self {
            Produced::Unknown => None,
            Produced::Listed(files) => files.get(in_flow).copied(),
            Produced::FromFileNames => named,
        }
    }
}

/// A failure screenshot or hierarchy waiting for its flow's `commands.json`.
#[derive(Debug, Clone)]
struct PendingFile {
    at: ClockSample,
    class: &'static str,
    /// Path relative to the debug output directory.
    rel: String,
    /// The 0-based step its file name gives.
    named: Option<u32>,
}

impl Flow {
    pub(crate) fn resolve(&self, suite_closed: bool) -> Resolution {
        claims::resolve(&self.claims, self.started, suite_closed)
    }
}

#[derive(Debug, Clone)]
struct Shard {
    number: u32,
    id: Uuid,
    device_session_id: Uuid,
    planned: Vec<String>,
    /// The device serial Maestro selected for the shard, from its log.
    serial: Option<String>,
}

/// Generates entity IDs: UUIDv7 live, deterministic in tests and `inspect`.
pub(crate) type IdGen = Box<dyn FnMut() -> Uuid + Send>;

/// The session.
pub(crate) struct Session {
    redactor: Redactor,
    workspace: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    new_id: IdGen,
    suite_id: Uuid,
    shards: Vec<Shard>,
    pub(crate) flows: Vec<Flow>,
    seq: u64,
    sequence_aborted: bool,
    closed: bool,
    signalled: bool,
    /// The Maestro build the session log named, once it has.
    maestro: Option<MaestroBuild>,
    pub(crate) unrecognized_stdout: u32,
    /// adb's latest device list; `None` until adb has pushed one (so no device is known absent).
    devices: Option<std::collections::BTreeMap<String, fleet_protocol::event::AdbState>>,
    out: Vec<EventEnvelope>,
    /// Files worth uploading that the observer hasn't taken yet.
    uploads: Vec<Candidate>,
    /// Devices a flow failed on since the observer last took them: their serial and device session
    /// (PLAN §6.5: the current activity is read on failure).
    failed_devices: Vec<(String, Uuid)>,
}

/// A file the observer may upload (PLAN §6.6): what it is and which flow it is evidence for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) class: ArtifactClass,
    /// Path relative to the debug output directory.
    pub(crate) rel: String,
    pub(crate) flow: Option<Uuid>,
    /// The step that produced it, as `artifacts[]` names it (see [`Session::register_file`]).
    pub(crate) step: Option<u32>,
    /// For a device logcat excerpt: the failing step whose window it keeps (issue #75).
    pub(crate) failure: Option<logcat::Failure>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("suite_id", &self.suite_id)
            .field("flows", &self.flows.len())
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(crate) fn set_workspace(
        &mut self,
        workspace: Option<std::path::PathBuf>,
        cwd: Option<std::path::PathBuf>,
    ) {
        self.workspace = workspace;
        self.cwd = cwd;
    }

    fn repo_path(&self, file: Option<&str>) -> Option<String> {
        use std::path::{Component, Path};
        let path = Path::new(file?);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.as_ref()?.join(path)
        };
        let mut normalized = std::path::PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        return None;
                    }
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        let root = self.workspace.as_ref()?;
        let relative = normalized.strip_prefix(root).ok()?;
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return None;
        }
        relative
            .to_str()
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    }

    /// Starts a session for a Maestro command line.
    pub(crate) fn new(
        argv: &[String],
        evidence: Option<EvidenceSources>,
        redactor: Redactor,
        mut new_id: IdGen,
        at: ClockSample,
    ) -> Self {
        let suite_id = new_id();
        let mut s = Self {
            redactor,
            workspace: None,
            cwd: None,
            new_id,
            suite_id,
            shards: Vec::new(),
            flows: Vec::new(),
            seq: 0,
            sequence_aborted: false,
            failed_devices: Vec::new(),
            closed: false,
            signalled: false,
            maestro: None,
            unrecognized_stdout: 0,
            devices: None,
            out: Vec::new(),
            uploads: Vec::new(),
        };
        let argv_redacted = argv
            .iter()
            .map(|a| s.redactor.redact(a).into_owned())
            .collect();
        s.emit(
            at,
            Subject::default(),
            SourceKind::AgentObserved,
            None,
            &EventBody::RunStarted(RunStarted {
                argv: argv_redacted,
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                evidence: evidence.map(|e| redact_evidence(e, &s.redactor)),
            }),
        );
        let inv = fleet_maestro::invocation::parse(argv);
        let subject = s.suite_subject();
        s.emit(
            at,
            subject,
            SourceKind::Declared,
            None,
            &EventBody::SuiteStarted(SuiteStarted {
                shard_mode: inv.shard_mode.unwrap_or(ShardMode::None),
                shards: inv.shards,
            }),
        );
        s
    }

    /// The Maestro build the session log named, if it has.
    pub(crate) fn maestro(&self) -> Option<&MaestroBuild> {
        self.maestro.as_ref()
    }

    /// Events produced so far, removed from the session.
    pub(crate) fn drain(&mut self) -> Vec<EventEnvelope> {
        std::mem::take(&mut self.out)
    }

    /// Files that appeared and are worth uploading, removed from the session.
    pub(crate) fn take_uploads(&mut self) -> Vec<Candidate> {
        std::mem::take(&mut self.uploads)
    }

    /// The redactor every outgoing text goes through.
    pub(crate) fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// A path label as it leaves the host: redacted (I6).
    pub(crate) fn label(&self, rel: &str) -> String {
        self.redactor.redact(rel).into_owned()
    }

    /// Journals how an upload ended (`artifact.uploaded` or `artifact.rejected`) against the flow
    /// it belongs to.
    pub(crate) fn artifact_result(
        &mut self,
        at: ClockSample,
        flow: Option<Uuid>,
        body: &EventBody,
    ) {
        let subject = flow
            .and_then(|id| self.flows.iter().position(|f| f.id == id))
            .map_or_else(Subject::default, |idx| self.flow_subject_at(idx));
        self.emit(at, subject, SourceKind::AgentObserved, None, body);
    }

    /// Feeds one observation.
    pub(crate) fn observe(&mut self, at: ClockSample, obs: Observation) {
        match obs {
            Observation::Stdout(line) => self.on_stdout(at, &line),
            Observation::Log(record) => self.on_log(at, &record),
            Observation::File { rel, contents } => self.on_file(at, &rel, contents.as_deref()),
            Observation::Signal(signal) => {
                self.signalled = true;
                self.emit(
                    at,
                    Subject::default(),
                    SourceKind::AgentObserved,
                    None,
                    &EventBody::RunSignal(RunSignal { signal }),
                );
            }
            Observation::Exited {
                code,
                signal,
                report,
            } => self.on_exit(at, code, signal, report),
            Observation::Report(bytes) => self.on_report(at, bytes.as_deref()),
            Observation::Devices(list) => self.on_devices(at, list),
            Observation::Plan(plan) => {
                let subject = self.suite_subject();
                self.emit(
                    at,
                    subject,
                    SourceKind::Declared,
                    None,
                    &EventBody::PlanAssigned(plan),
                );
            }
        }
    }

    /// Ends the run: open flows of a finished suite get their final finality.
    pub(crate) fn end(&mut self, at: ClockSample, output_bytes_dropped: u64) {
        // Files still waiting (no `commands.json` with commands came) keep the file-name link.
        for idx in 0..self.flows.len() {
            let pending = std::mem::take(&mut self.flows[idx].pending_files);
            for p in pending {
                self.register_file(p.at, idx, p.class, &p.rel, p.named);
            }
        }
        self.closed = true;
        self.emit(
            at,
            Subject::default(),
            SourceKind::AgentObserved,
            None,
            &EventBody::RunEnded(RunEnded {
                interrupted: self.signalled,
                output_bytes_dropped,
            }),
        );
    }

    /// Emits a `device.state_observed` for every device whose state changed, and `absent` for
    /// devices that left the list.
    fn on_devices(
        &mut self,
        at: ClockSample,
        list: Vec<(String, fleet_protocol::event::AdbState)>,
    ) {
        use fleet_protocol::event::{AdbState, DeviceStateObserved};
        let previous = self.devices.take().unwrap_or_default();
        let mut changes: Vec<(String, AdbState)> = list
            .iter()
            .filter(|(s, st)| previous.get(s) != Some(st))
            .cloned()
            .collect();
        changes.extend(
            previous
                .keys()
                .filter(|s| !list.iter().any(|(l, _)| l == *s))
                .map(|s| (s.clone(), AdbState::Absent)),
        );
        self.devices = Some(list.into_iter().collect());
        for (serial, state) in changes {
            self.emit(
                at,
                Subject::default(),
                SourceKind::AgentObserved,
                Some("adb".into()),
                &EventBody::DeviceStateObserved(DeviceStateObserved { serial, state }),
            );
        }
    }

    fn on_log(&mut self, at: ClockSample, record: &log::Record) {
        let parser = format!("maestro-log@{}", fleet_maestro::PROFILE);
        match log::classify(record) {
            Some(log::Event::ShardAssigned {
                shard,
                serial,
                flows,
            }) => {
                let suite_execution_id = Some(self.suite_id);
                let s = self.shard(shard);
                s.planned.clone_from(&flows);
                s.serial = Some(serial.clone());
                let subject = Subject {
                    suite_execution_id,
                    shard_id: Some(s.id),
                    device_session_id: Some(s.device_session_id),
                    ..Subject::default()
                };
                let planned = flows
                    .iter()
                    .map(|p| FlowRef {
                        name: stem(p),
                        path: Some(p.clone()),
                    })
                    .collect();
                self.emit(
                    at,
                    subject,
                    SourceKind::DebugLog,
                    Some(parser),
                    &EventBody::ShardAssigned(ShardAssigned {
                        shard_number: shard,
                        device_serial: serial,
                        planned,
                    }),
                );
            }
            Some(log::Event::FlowStarted { shard, name }) => {
                let shard = shard.unwrap_or(1);
                let idx = self.start_flow(shard, &name);
                let f = self.flows.get(idx).cloned();
                if let Some(f) = f {
                    let subject = self.flow_subject(&f);
                    self.emit(
                        at,
                        subject,
                        SourceKind::DebugLog,
                        Some(parser),
                        &EventBody::FlowStarted(FlowStarted {
                            flow: FlowRef {
                                name: f.name.clone(),
                                path: f.path.clone(),
                            },
                            shard_number: shard,
                            ordinal: f.ordinal,
                        }),
                    );
                }
            }
            Some(log::Event::Step {
                shard,
                description,
                state,
            }) => self.on_step(at, shard.unwrap_or(1), &description, state, parser),
            Some(log::Event::FlowCrashed { exception }) => {
                if let Some(idx) = self.open_flow(1, None).or_else(|| self.last_open_flow()) {
                    let message = exception.map(|e| self.clip(&e));
                    self.claim(
                        at,
                        idx,
                        SourceKind::DebugLog,
                        FlowOutcome::Failed,
                        None,
                        message,
                        parser,
                    );
                }
            }
            Some(log::Event::MaestroVersion { version }) => {
                // Once per run: Maestro writes its version at the start of the session log.
                if self.maestro.is_none() {
                    let build = MaestroBuild {
                        version: self.redactor.redact(&version).into_owned(),
                        profile_supported: fleet_maestro::supports(&version),
                    };
                    self.emit(
                        at,
                        Subject::default(),
                        SourceKind::DebugLog,
                        Some(parser),
                        &EventBody::MaestroDetected(build.clone()),
                    );
                    self.maestro = Some(build);
                }
            }
            Some(log::Event::RetryAttemptFailed { attempt }) => self.on_attempt_failed(attempt),
            None => {}
        }
    }

    /// `Attempt N failed for retry command`: the next run of the retry block's commands is attempt
    /// N + 1. The line has no shard, so it goes to the one flow that holds a retry block open; with
    /// several, which block failed is unknown, and their attempts become unknown too.
    fn on_attempt_failed(&mut self, attempt: u32) {
        let holders: Vec<usize> = self
            .flows
            .iter()
            .enumerate()
            .filter(|(_, f)| f.started && f.open_steps.iter().any(|s| s.retry.is_some()))
            .map(|(i, _)| i)
            .collect();
        let single = holders.len() == 1;
        for (i, f) in self.flows.iter_mut().enumerate() {
            if !holders.contains(&i) {
                continue;
            }
            if single {
                let innermost = f.open_steps.iter_mut().rev().find(|s| s.retry.is_some());
                if let Some(block) = innermost {
                    block.retry = Some(match block.retry {
                        Some(Retry::Attempt(n)) if n == attempt => Retry::Attempt(n + 1),
                        _ => Retry::Unknown,
                    });
                }
            } else {
                for block in f.open_steps.iter_mut().filter(|s| s.retry.is_some()) {
                    block.retry = Some(Retry::Unknown);
                }
            }
        }
    }

    fn on_step(
        &mut self,
        at: ClockSample,
        shard: u32,
        description: &str,
        state: log::StepState,
        parser: String,
    ) {
        let description = self.redactor.redact(description).into_owned();
        let Some(idx) = self.step_flow(shard, &description, state) else {
            return;
        };
        let Some(f) = self.flows.get_mut(idx) else {
            return;
        };
        let status = match state {
            log::StepState::Pending => return,
            log::StepState::Running => {
                let index = f.steps;
                f.steps += 1;
                // The steps still open enclose this one: Maestro's log nests them by their RUNNING
                // and COMPLETED lines, and the depth matches each command's `depth` in
                // `commands.json` (issue #38). A top-level step has no depth, parent or attempt.
                let depth = u32::try_from(f.open_steps.len()).ok().filter(|d| *d > 0);
                let parent = f.open_steps.last().map(|s| StepRef { index: s.index });
                let attempt = match f.open_steps.iter().rev().find_map(|s| s.retry) {
                    Some(Retry::Attempt(n)) => Some(n),
                    _ => None,
                };
                let retry = log::is_retry_block(&description).then_some(Retry::Attempt(1));
                f.open_steps.push(OpenStep {
                    index,
                    description: description.clone(),
                    retry,
                });
                let mut subject = self.flow_subject_at(idx);
                subject.step = Some(StepRef { index });
                self.emit(
                    at,
                    subject,
                    SourceKind::DebugLog,
                    Some(parser),
                    &EventBody::StepStarted(StepStarted {
                        description,
                        depth,
                        parent,
                        attempt,
                    }),
                );
                return;
            }
            log::StepState::Completed => StepStatus::Completed,
            log::StepState::Failed => StepStatus::Failed,
            log::StepState::Warned => StepStatus::Warned,
            log::StepState::Skipped => StepStatus::Skipped,
        };
        // Blocks nest (runFlow, retry, repeat), so the step that ends is the innermost open one with
        // this description; a step still open above it never got an end and is dropped with it.
        // A step that ends without a RUNNING line (SKIPPED) gets its own index.
        let index = if let Some(pos) = f
            .open_steps
            .iter()
            .rposition(|s| s.description == description)
        {
            let index = f.open_steps[pos].index;
            f.open_steps.truncate(pos);
            index
        } else {
            let i = f.steps;
            f.steps += 1;
            i
        };
        let mut subject = self.flow_subject_at(idx);
        subject.step = Some(StepRef { index });
        self.emit(
            at,
            subject,
            SourceKind::DebugLog,
            Some(parser),
            &EventBody::StepEnded(StepEnded { status }),
        );
    }

    /// The flow a step line belongs to. An end can arrive after a later flow's stdout result (the
    /// log lags stdout), so an end goes to the newest started flow that still holds this step open.
    fn step_flow(&self, shard: u32, description: &str, state: log::StepState) -> Option<usize> {
        if state != log::StepState::Running {
            let holder = self.flows.iter().rposition(|f| {
                f.shard == shard
                    && f.started
                    && f.open_steps.iter().any(|s| s.description == description)
            });
            if holder.is_some() {
                return holder;
            }
        }
        self.open_flow(shard, None)
    }

    fn on_stdout(&mut self, at: ClockSample, line: &str) {
        let parser = format!("maestro-console@{}", fleet_maestro::PROFILE);
        match stdout::classify(line) {
            stdout::Line::FlowResult {
                shard,
                name,
                passed,
                duration_ms,
                message,
            } => {
                let shard = shard.unwrap_or(1);
                let idx = self
                    .open_flow(shard, Some(&name))
                    .unwrap_or_else(|| self.start_flow(shard, &name));
                let outcome = if passed {
                    FlowOutcome::Passed
                } else {
                    FlowOutcome::Failed
                };
                let message = message.map(|m| self.clip(&m));
                self.claim(
                    at,
                    idx,
                    SourceKind::LiveConsole,
                    outcome,
                    duration_ms,
                    message,
                    parser,
                );
            }
            stdout::Line::SequenceAborted { .. } => self.sequence_aborted = true,
            stdout::Line::Unrecognized => self.unrecognized_stdout += 1,
            stdout::Line::Summary { .. } | stdout::Line::ShardPlan { .. } | stdout::Line::Noise => {
            }
        }
    }

    fn on_file(&mut self, at: ClockSample, rel: &str, contents: Option<&[u8]>) {
        match bundle::classify(rel) {
            bundle::Entry::Commands { flow_dir } => {
                let Some(bytes) = contents else { return };
                let (name, shard) = bundle::split_shard_suffix(&flow_dir);
                let Some(idx) = self.find_flow(shard.unwrap_or(1), name) else {
                    return;
                };
                self.on_commands(at, rel, idx, bytes);
                let flow = self.flows.get(idx).map(|f| f.id);
                self.uploads.push(Candidate {
                    class: ArtifactClass::Commands,
                    rel: rel.to_owned(),
                    flow,
                    step: None,
                    failure: None,
                });
            }
            bundle::Entry::Manifest { flow_dir } => {
                let (name, shard) = bundle::split_shard_suffix(&flow_dir);
                let Some(idx) = self.find_flow(shard.unwrap_or(1), name) else {
                    return;
                };
                let pending = self.flows.get_mut(idx).and_then(|f| {
                    f.manifest_seen = true;
                    f.pending_pass.take()
                });
                if let Some(parser) = pending {
                    self.claim(
                        at,
                        idx,
                        SourceKind::ArtifactFile,
                        FlowOutcome::Passed,
                        None,
                        None,
                        parser,
                    );
                }
            }
            bundle::Entry::StepScreenshot { flow_dir, step }
            | bundle::Entry::StepHierarchy { flow_dir, step } => {
                let (name, shard) = bundle::split_shard_suffix(&flow_dir);
                let class = if rel.contains("/screenshots/") {
                    "failure_screenshot"
                } else {
                    "failure_hierarchy"
                };
                if let Some(idx) = self.find_flow(shard.unwrap_or(1), name) {
                    let named = step.checked_sub(1);
                    match self.flows.get_mut(idx) {
                        Some(f) if matches!(f.produced, Produced::Unknown) => {
                            f.pending_files.push(PendingFile {
                                at,
                                class,
                                rel: rel.to_owned(),
                                named,
                            });
                        }
                        Some(f) => {
                            let step = f.produced.step_of(rel, named);
                            self.register_file(at, idx, class, rel, step);
                        }
                        None => {}
                    }
                }
            }
            bundle::Entry::CrashReport { flow_dir } => {
                let (name, shard) = bundle::split_shard_suffix(&flow_dir);
                let Some(idx) = self.find_flow(shard.unwrap_or(1), name) else {
                    return;
                };
                let Some(bytes) = contents else { return };
                let Some(crash) = crash::parse_crash_report(&String::from_utf8_lossy(bytes)) else {
                    return;
                };
                let body = EventBody::AppCrashed(AppCrashed {
                    package: self.redactor.redact(&crash.package).into_owned(),
                    exception: self.clip(&crash.exception),
                });
                let subject = self.flow_subject_at(idx);
                let parser = format!("crash-report@{}", fleet_maestro::PROFILE);
                self.emit(at, subject, SourceKind::ArtifactFile, Some(parser), &body);
            }
            bundle::Entry::FlowDir { .. } | bundle::Entry::Other => {}
        }
    }

    /// Turns a flow's `commands.json` into a claim or a warning (fail closed, I3).
    fn on_commands(&mut self, at: ClockSample, rel: &str, idx: usize, bytes: &[u8]) {
        let parsed = bundle::parse_commands(bytes);
        if let bundle::Commands::Final(cmds)
        | bundle::Commands::Aborted(cmds)
        | bundle::Commands::Unverified(cmds) = &parsed
        {
            self.learn_produced(idx, cmds);
        }
        match parsed {
            bundle::Commands::Final(cmds) => {
                let failed = cmds.iter().any(|c| c.depth == 0 && c.status == "FAILED");
                let parser = format!("maestro-bundle@{}", fleet_maestro::PROFILE);
                if failed {
                    // Failure evidence needs no completion proof.
                    self.claim(
                        at,
                        idx,
                        SourceKind::ArtifactFile,
                        FlowOutcome::Failed,
                        None,
                        None,
                        parser,
                    );
                    self.queue_logcat(at, idx, rel, &cmds);
                } else if self.flows.get(idx).is_some_and(|f| f.manifest_seen) {
                    self.claim(
                        at,
                        idx,
                        SourceKind::ArtifactFile,
                        FlowOutcome::Passed,
                        None,
                        None,
                        parser,
                    );
                } else if let Some(f) = self.flows.get_mut(idx) {
                    // Wait for the manifest: without it the flow may have been cut short.
                    f.pending_pass = Some(parser);
                }
            }
            other => {
                let (code, why) = match other {
                    bundle::Commands::Unverified(_) => (
                        "commands_unknown_status",
                        "has a command status Fleet does not recognise",
                    ),
                    bundle::Commands::Empty => ("commands_not_final", "has no commands"),
                    _ => (
                        "commands_not_final",
                        "is incomplete: the flow was cut short",
                    ),
                };
                let subject = self.flow_subject_at(idx);
                self.emit(
                    at,
                    subject,
                    SourceKind::ArtifactFile,
                    None,
                    &EventBody::AgentWarning(AgentWarning {
                        code: code.into(),
                        message: format!("{rel} {why}"),
                    }),
                );
            }
        }
    }

    /// Queues the device logcat excerpt of a failed flow: the window around its last failed
    /// command, which is the step that ended the flow (issue #75). The excerpt is cut at upload,
    /// when the log is read. A failed command without timing gets no excerpt, and says so.
    fn queue_logcat(
        &mut self,
        at: ClockSample,
        idx: usize,
        rel: &str,
        cmds: &[bundle::CommandRecord],
    ) {
        let Some(failing) = cmds.iter().rev().find(|c| c.status == "FAILED") else {
            return;
        };
        let timing = failing.timestamp_ms.zip(failing.duration_ms);
        let Some((start, duration)) = timing else {
            self.warn(
                at,
                "logcat_window_unknown",
                &format!(
                    "{rel} has no timing for its failed command, so no device log excerpt is kept"
                ),
            );
            return;
        };
        let start_ms = i64::try_from(start).unwrap_or(i64::MAX);
        let end_ms = start_ms.saturating_add(i64::try_from(duration).unwrap_or(i64::MAX));
        let flow_dir = rel.split_once('/').map_or(rel, |(dir, _)| dir);
        let flow = self.flows.get(idx).map(|f| f.id);
        self.uploads.push(Candidate {
            class: ArtifactClass::DeviceLogcat,
            rel: format!("{flow_dir}/logs/device-logcat.txt"),
            flow,
            step: Some(failing.sequence),
            failure: Some(logcat::Failure { start_ms, end_ms }),
        });
    }

    /// Records which step produced each of a flow's files, then attaches the files that waited for
    /// it. A file `artifacts[]` does not list gets no step (I3: nothing is guessed).
    fn learn_produced(&mut self, idx: usize, cmds: &[bundle::CommandRecord]) {
        let Some(f) = self.flows.get_mut(idx) else {
            return;
        };
        f.produced = match bundle::produced_files(cmds) {
            Some(files) => Produced::Listed(files),
            None => Produced::FromFileNames,
        };
        let pending = std::mem::take(&mut f.pending_files);
        for p in pending {
            let step = self
                .flows
                .get(idx)
                .and_then(|f| f.produced.step_of(&p.rel, p.named));
            self.register_file(p.at, idx, p.class, &p.rel, step);
        }
    }

    /// Journals a failure screenshot or hierarchy, against the step that produced it when known.
    /// A screenshot is queued for upload here, not when it appears, so the upload request carries
    /// the step too (issue #76).
    fn register_file(
        &mut self,
        at: ClockSample,
        idx: usize,
        class: &str,
        rel: &str,
        step: Option<u32>,
    ) {
        let mut subject = self.flow_subject_at(idx);
        subject.step = step.map(|index| StepRef { index });
        let upload = match class {
            "failure_screenshot" => Some(ArtifactClass::FailureScreenshot),
            "failure_hierarchy" => Some(ArtifactClass::FailureHierarchy),
            _ => None,
        };
        if let Some(class) = upload {
            self.uploads.push(Candidate {
                class,
                rel: rel.to_owned(),
                flow: self.flows.get(idx).map(|f| f.id),
                step,
                failure: None,
            });
        }
        self.emit(
            at,
            subject,
            SourceKind::AgentObserved,
            None,
            &EventBody::ArtifactRegistered(ArtifactRegistered {
                class: class.into(),
                path: rel.to_owned(),
            }),
        );
    }

    fn on_exit(
        &mut self,
        at: ClockSample,
        code: Option<i32>,
        signal: Option<i32>,
        report: ReportState,
    ) {
        // Failures seen directly (stdout, bundle, log), not through the report.
        let flow_failed_observed = self.flows.iter().any(|f| {
            f.claims
                .iter()
                .any(|c| c.outcome() == FlowOutcome::Failed && c.source() != SourceKind::JunitFinal)
        });
        let class = fleet_maestro::exit_class(code, signal, report, flow_failed_observed);
        self.emit(
            at,
            self.suite_subject(),
            SourceKind::ProcessExit,
            None,
            &EventBody::ProcessExited(ProcessExited {
                code,
                signal,
                class,
            }),
        );
        if self.signalled || signal.is_some() || matches!(code, Some(130 | 143)) {
            for idx in 0..self.flows.len() {
                let open = self
                    .flows
                    .get(idx)
                    .is_some_and(|f| f.started && f.claims.is_empty());
                if open {
                    self.claim(
                        at,
                        idx,
                        SourceKind::ProcessExit,
                        FlowOutcome::Interrupted,
                        None,
                        None,
                        "agent".into(),
                    );
                }
            }
        }
    }

    fn on_report(&mut self, at: ClockSample, bytes: Option<&[u8]>) {
        let parser = format!("maestro-junit@{}", fleet_maestro::PROFILE);
        let report = bytes.map(junit::parse);
        let status = report.as_ref().map_or(ReportStatus::Absent, |r| r.status);
        let (suites, tests, failures) = report.as_ref().map_or((0, 0, 0), |r| {
            let tests: usize = r.suites.iter().map(|s| s.cases.len()).sum();
            let failures = r
                .suites
                .iter()
                .flat_map(|s| &s.cases)
                .filter(|c| c.status.as_deref() == Some("ERROR"))
                .count();
            (r.suites.len(), tests, failures)
        });
        self.emit(
            at,
            self.suite_subject(),
            SourceKind::JunitFinal,
            Some(parser.clone()),
            &EventBody::ReportParsed(ReportParsed {
                status,
                suites: u32::try_from(suites).unwrap_or(u32::MAX),
                tests: u32::try_from(tests).unwrap_or(u32::MAX),
                failures: u32::try_from(failures).unwrap_or(u32::MAX),
            }),
        );
        let Some(report) = report.filter(|r| r.status != ReportStatus::Invalid) else {
            if self.sequence_aborted {
                self.mark_unstarted_skipped(at);
            }
            return;
        };
        self.apply_report(at, &report, &parser);
        if self.sequence_aborted {
            self.mark_unstarted_skipped(at);
        }
    }

    /// Matches the report's test cases to live flows and records a `junit_final` claim for each.
    fn apply_report(&mut self, at: ClockSample, report: &junit::Report, parser: &str) {
        let max_shard = self
            .flows
            .iter()
            .map(|f| f.shard)
            .chain(self.shards.iter().map(|s| s.number))
            .max()
            .unwrap_or(1);
        let live: Vec<Vec<FlowKey>> = (1..=max_shard)
            .map(|n| {
                self.flows
                    .iter()
                    .filter(|f| f.shard == n)
                    .map(|f| FlowKey {
                        name: f.name.clone(),
                        path: f.path.clone(),
                    })
                    .collect()
            })
            .collect();
        let cases: Vec<Vec<FlowKey>> = report
            .suites
            .iter()
            .map(|s| {
                s.cases
                    .iter()
                    .map(|c| FlowKey {
                        name: c.name.clone(),
                        path: c.file.clone(),
                    })
                    .collect()
            })
            .collect();
        for m in reconcile::reconcile(&live, &cases) {
            let Some(case) = report.suites.get(m.suite).and_then(|s| s.cases.get(m.case)) else {
                continue;
            };
            let shard = u32::try_from(m.suite).unwrap_or(u32::MAX).saturating_add(1);
            let idx = match m.live {
                Some((s, f)) => self
                    .flows
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| x.shard as usize == s + 1)
                    .nth(f)
                    .map(|(i, _)| i),
                None => None,
            }
            .unwrap_or_else(|| self.start_flow_unstarted(shard, &case.name, case.file.clone()));
            let outcome = junit::outcome(case.status.as_deref());
            let repo_path = self
                .repo_path(case.file.as_deref())
                .map(|p| self.redactor.redact(&p).into_owned());
            let mut junit_case = case.clone();
            junit_case.file = junit_case
                .file
                .map(|p| self.redactor.redact(&p).into_owned());
            junit_case.failure = junit_case.failure.map(|f| self.clip(&f));
            if let Ok(c) = Claim::new(SourceKind::JunitFinal, outcome, self.seq + 1)
                && let Some(f) = self.flows.get_mut(idx)
            {
                f.claims.push(c);
                if f.failure.is_none() {
                    f.failure.clone_from(&junit_case.failure);
                }
            }
            let subject = self.flow_subject_at(idx);
            self.emit(
                at,
                subject,
                SourceKind::JunitFinal,
                Some(parser.to_owned()),
                &EventBody::FlowReported(FlowReported {
                    repo_path,
                    testsuite_index: u32::try_from(m.suite).unwrap_or(u32::MAX),
                    testcase_index: u32::try_from(m.case).unwrap_or(u32::MAX),
                    matched: m.method,
                    junit: junit_case,
                    outcome,
                }),
            );
        }
    }

    /// Planned flows that never started after an `executionOrder` abort were skipped by Maestro.
    fn mark_unstarted_skipped(&mut self, at: ClockSample) {
        let planned: Vec<(u32, String)> = self
            .shards
            .iter()
            .flat_map(|s| s.planned.iter().map(move |p| (s.number, p.clone())))
            .filter(|(n, p)| {
                !self.flows.iter().any(|f| {
                    f.shard == *n
                        && f.path
                            .as_deref()
                            .is_some_and(|fp| reconcile::same_file(Some(fp), Some(p)))
                })
            })
            .collect();
        for (shard, path) in planned {
            let idx = self.start_flow_unstarted(shard, &stem(&path), Some(path));
            self.claim(
                at,
                idx,
                SourceKind::Declared,
                FlowOutcome::Skipped,
                None,
                None,
                "agent".into(),
            );
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one claim: who, what, and the evidence behind it"
    )]
    fn claim(
        &mut self,
        at: ClockSample,
        idx: usize,
        source: SourceKind,
        outcome: FlowOutcome,
        duration_ms: Option<u64>,
        message: Option<String>,
        parser: String,
    ) {
        let order = self.seq + 1;
        let Ok(claim) = Claim::new(source, outcome, order) else {
            return;
        };
        let Some(f) = self.flows.get_mut(idx) else {
            return;
        };
        f.claims.push(claim);
        if f.failure.is_none() {
            f.failure.clone_from(&message);
        }
        // A flow that never started has no `flow.started` to name it.
        let flow = (!f.started).then(|| FlowRef {
            name: f.name.clone(),
            path: f.path.clone(),
        });
        let subject = self.flow_subject_at(idx);
        if outcome == FlowOutcome::Failed {
            self.note_failed_device(idx);
        }
        self.emit(
            at,
            subject,
            source,
            Some(parser),
            &EventBody::FlowEnded(FlowEnded {
                outcome,
                duration_ms,
                failure: message.map(|message| FailureInfo { message }),
                flow,
            }),
        );
    }

    /// Remembers the device of flow `idx`'s shard as failed, once per serial until taken.
    fn note_failed_device(&mut self, idx: usize) {
        let Some(flow) = self.flows.get(idx) else {
            return;
        };
        let Some(shard) = self.shards.iter().find(|s| s.number == flow.shard) else {
            return;
        };
        let Some(serial) = &shard.serial else {
            return;
        };
        let entry = (serial.clone(), shard.device_session_id);
        if !self.failed_devices.contains(&entry) {
            self.failed_devices.push(entry);
        }
    }

    /// The devices a flow failed on since the last call, each once, and forgets them.
    pub(crate) fn take_failed_devices(&mut self) -> Vec<(String, Uuid)> {
        std::mem::take(&mut self.failed_devices)
    }

    /// The device each shard Maestro selected, once per serial (the first shard that selected
    /// it), with the device session that shard's events name. Only these devices are probed.
    pub(crate) fn driven_devices(&self) -> Vec<(String, Uuid)> {
        let mut out: Vec<(String, Uuid)> = Vec::new();
        for shard in &self.shards {
            if let Some(serial) = &shard.serial
                && !out.iter().any(|(s, _)| s == serial)
            {
                out.push((serial.clone(), shard.device_session_id));
            }
        }
        out
    }

    /// adb's state for `serial` in the latest device list: `Absent` when the list lacks it, and
    /// `None` before any list has arrived.
    pub(crate) fn adb_state(&self, serial: &str) -> Option<fleet_protocol::event::AdbState> {
        self.devices.as_ref().map(|list| {
            list.get(serial)
                .copied()
                .unwrap_or(fleet_protocol::event::AdbState::Absent)
        })
    }

    fn shard(&mut self, number: u32) -> &mut Shard {
        if !self.shards.iter().any(|s| s.number == number) {
            let (id, device_session_id) = ((self.new_id)(), (self.new_id)());
            self.shards.push(Shard {
                number,
                id,
                device_session_id,
                planned: Vec::new(),
                serial: None,
            });
        }
        let pos = self
            .shards
            .iter()
            .position(|s| s.number == number)
            .unwrap_or_default();
        let last = self.shards.len() - 1;
        let pos = pos.min(last);
        self.shards
            .get_mut(pos)
            .expect("a shard was just inserted, so the list is not empty")
    }

    /// Records a flow start; its path comes from the shard's plan when the name matches a file.
    fn start_flow(&mut self, shard: u32, name: &str) -> usize {
        let idx = self.start_flow_unstarted(shard, name, None);
        if let Some(f) = self.flows.get_mut(idx) {
            f.started = true;
        }
        idx
    }

    fn start_flow_unstarted(&mut self, shard: u32, name: &str, path: Option<String>) -> usize {
        let ordinal = u32::try_from(self.flows.iter().filter(|f| f.shard == shard).count())
            .unwrap_or(u32::MAX);
        let planned_path = path.or_else(|| {
            self.shards
                .iter()
                .find(|s| s.number == shard)
                .and_then(|s| s.planned.iter().find(|p| stem(p) == name).cloned())
        });
        let id = (self.new_id)();
        self.flows.push(Flow {
            id,
            shard,
            name: name.to_owned(),
            path: planned_path,
            ordinal,
            started: false,
            claims: Vec::new(),
            steps: 0,
            open_steps: Vec::new(),
            failure: None,
            manifest_seen: false,
            pending_pass: None,
            produced: Produced::Unknown,
            pending_files: Vec::new(),
        });
        self.flows.len() - 1
    }

    /// The most recent started flow on `shard` without a live result (optionally with `name`).
    fn open_flow(&self, shard: u32, name: Option<&str>) -> Option<usize> {
        self.flows.iter().rposition(|f| {
            f.shard == shard
                && f.started
                && name.is_none_or(|n| n == f.name)
                && !f
                    .claims
                    .iter()
                    .any(|c| c.source() == SourceKind::LiveConsole)
        })
    }

    fn last_open_flow(&self) -> Option<usize> {
        self.flows
            .iter()
            .rposition(|f| f.started && f.claims.is_empty())
    }

    fn find_flow(&self, shard: u32, dir_name: &str) -> Option<usize> {
        // Bundle folder names are sanitised flow names; match the latest flow with that name.
        self.flows
            .iter()
            .rposition(|f| f.shard == shard && sanitize(&f.name) == dir_name)
    }

    fn suite_subject(&self) -> Subject {
        Subject {
            suite_execution_id: Some(self.suite_id),
            ..Subject::default()
        }
    }

    fn flow_subject_at(&self, idx: usize) -> Subject {
        self.flows
            .get(idx)
            .map_or_else(|| self.suite_subject(), |f| self.flow_subject(f))
    }

    fn flow_subject(&self, f: &Flow) -> Subject {
        let shard = self.shards.iter().find(|s| s.number == f.shard);
        Subject {
            suite_execution_id: Some(self.suite_id),
            shard_id: shard.map(|s| s.id),
            device_session_id: shard.map(|s| s.device_session_id),
            flow_execution_id: Some(f.id),
            step: None,
        }
    }

    fn clip(&self, text: &str) -> String {
        let redacted = self.redactor.redact(text);
        redacted.chars().take(MAX_MESSAGE).collect()
    }

    /// Emits an `agent.warning` for the run, message redacted (I6).
    pub(crate) fn warn(&mut self, at: ClockSample, code: &str, message: &str) {
        let message = self.redactor.redact(message).into_owned();
        self.emit(
            at,
            Subject::default(),
            SourceKind::AgentObserved,
            None,
            &EventBody::AgentWarning(AgentWarning {
                code: code.into(),
                message,
            }),
        );
    }

    fn emit(
        &mut self,
        at: ClockSample,
        subject: Subject,
        kind: SourceKind,
        parser: Option<String>,
        body: &EventBody,
    ) {
        self.seq += 1;
        let mut src = Provenance::new(kind);
        src.parser = parser;
        self.out
            .push(EventEnvelope::new(self.seq, at, subject, src, body));
    }

    /// The resolved state of every flow, for the end-of-run summary and `inspect`.
    pub(crate) fn summary(&self) -> Vec<(Flow, Resolution)> {
        self.flows
            .iter()
            .map(|f| (f.clone(), f.resolve(self.closed)))
            .collect()
    }
}

/// `flows/login.yaml` → `login`.
/// Paths go off the host, so they are redacted like the command line (I6).
fn redact_evidence(mut e: EvidenceSources, redactor: &Redactor) -> EvidenceSources {
    let clean = |p: String| redactor.redact(&p).into_owned();
    e.report.path = e.report.path.map(clean);
    e.debug_output = e.debug_output.map(clean);
    e
}

fn stem(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".yaml")
        .or_else(|| file.strip_suffix(".yml"))
        .unwrap_or(file)
        .to_owned()
}

/// Maestro's folder-name sanitising: `/` becomes `_`.
fn sanitize(name: &str) -> String {
    name.replace('/', "_")
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    const GOOD: &[u8] = br#"[{"metadata":{"status":"COMPLETED","sequenceNumber":0,"depth":0}}]"#;
    const FAILED: &[u8] = br#"[{"metadata":{"status":"FAILED","sequenceNumber":0,"depth":0}}]"#;

    fn at() -> ClockSample {
        ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        }
    }

    #[test]
    fn a_warning_is_redacted() {
        let mut s = Session::new(
            &["maestro".into(), "test".into()],
            None,
            Redactor::new(["hunter2-secret".to_owned()]),
            Box::new(|| Uuid::from_u128(1)),
            at(),
        );
        s.drain();
        s.warn(at(), "report_stale", "/tmp/hunter2-secret/report.xml old");
        let json = serde_json::to_string(&s.drain()).unwrap();
        assert!(
            json.contains("report_stale") && !json.contains("hunter2-secret"),
            "{json}"
        );
    }

    #[test]
    fn run_started_carries_the_evidence_description_redacted() {
        use fleet_protocol::event::{ReportReason, ReportSource};
        let evidence = EvidenceSources {
            report: ReportSource {
                path: Some("/tmp/hunter2-secret/report.xml".into()),
                reason: ReportReason::UserJunit,
            },
            debug_output: None,
            maestro_log: true,
        };
        let mut s = Session::new(
            &["maestro".into(), "test".into()],
            Some(evidence),
            Redactor::new(["hunter2-secret".to_owned()]),
            Box::new(|| Uuid::from_u128(1)),
            at(),
        );
        let first = s.drain().remove(0);
        let json = serde_json::to_string(&first).unwrap();
        assert!(!json.contains("hunter2-secret"), "I6: {json}");
        assert!(json.contains(r#""reason":"user_junit""#), "{json}");
    }

    /// A session with one flow `pass` that Maestro printed no result for yet.
    fn session() -> Session {
        let mut n = 0u128;
        let ids: IdGen = Box::new(move || {
            n += 1;
            Uuid::from_u128(n)
        });
        let mut s = Session::new(
            &["maestro".into(), "test".into()],
            None,
            Redactor::new([]),
            ids,
            at(),
        );
        s.start_flow(1, "pass");
        s
    }

    fn file(s: &mut Session, rel: &str, contents: Option<&[u8]>) {
        s.observe(
            at(),
            Observation::File {
                rel: rel.into(),
                contents: contents.map(<[u8]>::to_vec),
            },
        );
    }

    fn artifact_outcomes(s: &Session) -> Vec<FlowOutcome> {
        s.flows[0]
            .claims
            .iter()
            .filter(|c| c.source() == SourceKind::ArtifactFile)
            .map(Claim::outcome)
            .collect()
    }

    fn warnings(s: &mut Session) -> Vec<String> {
        s.drain()
            .into_iter()
            .filter(|e| e.kind == "agent.warning")
            .filter_map(|e| e.data["code"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn nothing_but_a_terminal_commands_file_with_a_manifest_passes() {
        for bad in [
            &b"[]"[..],
            b"",
            br#"[{"metadata":{"status":"ERROR","sequenceNumber":0,"depth":0}}]"#,
            br#"[{"metadata":{"sequenceNumber":0,"depth":0}}]"#,
            br#"[{"metadata":{"status":"RUNNING","sequenceNumber":0,"depth":0}}]"#,
        ] {
            let mut s = session();
            file(&mut s, "pass/commands.json", Some(bad));
            file(&mut s, "pass/manifest.json", None);
            assert!(artifact_outcomes(&s).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn unknown_status_is_reported_as_drift() {
        let mut s = session();
        let bad = br#"[{"metadata":{"status":"ERROR","sequenceNumber":0,"depth":0}}]"#;
        file(&mut s, "pass/commands.json", Some(bad));
        assert!(warnings(&mut s).contains(&"commands_unknown_status".to_owned()));
    }

    #[test]
    fn a_valid_commands_file_needs_the_manifest_to_pass() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(GOOD));
        assert!(artifact_outcomes(&s).is_empty(), "no manifest yet");
        file(&mut s, "pass/manifest.json", None);
        assert_eq!(artifact_outcomes(&s), [FlowOutcome::Passed]);
    }

    #[test]
    fn a_manifest_seen_first_also_passes() {
        let mut s = session();
        file(&mut s, "pass/manifest.json", None);
        file(&mut s, "pass/commands.json", Some(GOOD));
        assert_eq!(artifact_outcomes(&s), [FlowOutcome::Passed]);
    }

    #[test]
    fn failure_needs_no_manifest() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(FAILED));
        assert_eq!(artifact_outcomes(&s), [FlowOutcome::Failed]);
    }

    /// A flow that fails names its shard's device once, until the observer takes it (PLAN §6.5:
    /// the activity is read on failure). A device that never failed is not named.
    #[test]
    fn a_failed_flow_names_its_device_until_it_is_taken() {
        let mut s = session();
        log_line(
            &mut s,
            "maestro.cli.command.TestCommand",
            "[shard 1] Selected device emulator-5584 using port 54235 with execution plan ExecutionPlan(flowsToRun=[/a/pass.yaml], sequence=FlowSequence(flows=[], continueOnFailure=true))",
        );
        assert!(s.take_failed_devices().is_empty(), "nothing failed yet");
        file(&mut s, "pass/commands.json", Some(FAILED));
        let failed = s.take_failed_devices();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, "emulator-5584");
        assert!(s.take_failed_devices().is_empty(), "taken once");
    }

    #[test]
    fn only_the_devices_maestro_selected_are_driven_and_adb_states_come_from_its_list() {
        let mut s = session();
        assert_eq!(
            s.adb_state("emulator-5584"),
            None,
            "no list yet: nothing is absent"
        );
        s.observe(
            at(),
            Observation::Devices(vec![
                (
                    "emulator-5584".into(),
                    fleet_protocol::event::AdbState::Device,
                ),
                ("R58M".into(), fleet_protocol::event::AdbState::Device),
            ]),
        );
        assert!(
            s.driven_devices().is_empty(),
            "a device nobody selected is not probed"
        );
        log_line(
            &mut s,
            "maestro.cli.command.TestCommand",
            "[shard 1] Selected device emulator-5584 using port 54235 with execution plan ExecutionPlan(flowsToRun=[/a/pass.yaml], sequence=FlowSequence(flows=[], continueOnFailure=true))",
        );
        let driven = s.driven_devices();
        assert_eq!(driven.len(), 1);
        assert_eq!(driven[0].0, "emulator-5584");
        assert_eq!(
            Some(driven[0].1),
            s.shards.first().map(|sh| sh.device_session_id),
            "the samples belong to the device session the shard event names"
        );
        assert_eq!(
            s.adb_state("emulator-5584"),
            Some(fleet_protocol::event::AdbState::Device)
        );
        s.observe(at(), Observation::Devices(Vec::new()));
        assert_eq!(
            s.adb_state("emulator-5584"),
            Some(fleet_protocol::event::AdbState::Absent),
            "gone from adb's list"
        );
    }

    fn log_line(s: &mut Session, logger: &str, message: &str) {
        s.observe(
            at(),
            Observation::Log(log::Record {
                unix_ms: None,
                level: "INFO".into(),
                logger: logger.into(),
                message: message.into(),
                continuation: Vec::new(),
            }),
        );
    }

    /// Stdout can report a later flow before the log delivers an earlier flow's last step end.
    // UNVERIFIED: synthetic interleaving of stdout ahead of maestro.log
    #[test]
    fn a_late_step_end_pairs_with_the_flow_that_holds_it_open() {
        const SUITE: &str = "maestro.cli.runner.TestSuiteInteractor";
        const CONSOLE: &str = "maestro.cli.runner.CliConsoleListener";
        let mut n = 0u128;
        let ids: IdGen = Box::new(move || {
            n += 1;
            Uuid::from_u128(n)
        });
        let mut s = Session::new(
            &["maestro".into(), "test".into()],
            None,
            Redactor::new([]),
            ids,
            at(),
        );
        log_line(&mut s, SUITE, "Running flow A");
        log_line(&mut s, CONSOLE, "Tap on \"X\" RUNNING");
        s.observe(at(), Observation::Stdout("[Passed] A (1s)".into()));
        s.observe(at(), Observation::Stdout("[Passed] B (1s)".into()));
        log_line(&mut s, CONSOLE, "Tap on \"X\" COMPLETED");
        let (a, b) = (s.flows[0].id, s.flows[1].id);
        let events = s.drain();

        let ended: Vec<_> = events.iter().filter(|e| e.kind == "step.ended").collect();
        assert_eq!(ended.len(), 1, "A's end was not paired: {events:?}");
        assert_eq!(ended[0].subject.flow_execution_id, Some(a));
        assert_eq!(ended[0].subject.step.as_ref().map(|r| r.index), Some(0));
        assert!(
            events
                .iter()
                .all(|e| e.subject.flow_execution_id != Some(b) || e.subject.step.is_none()),
            "B got a step: {events:?}"
        );
    }

    /// A failed command 2 whose `artifacts[]` names a screenshot the file name would call step 4.
    const LISTED: &[u8] = br#"[{"metadata":{"status":"COMPLETED","sequenceNumber":0,"depth":0}},
        {"metadata":{"status":"COMPLETED","sequenceNumber":1,"depth":0}},
        {"metadata":{"status":"FAILED","sequenceNumber":2,"depth":0,
          "artifacts":[{"type":"SCREENSHOT","path":"screenshots/step-004-odd.png"}]}}]"#;

    /// Each `artifact.registered` of the session as `(file, step index)`.
    fn registered(s: &mut Session) -> Vec<(String, Option<u32>)> {
        s.drain()
            .into_iter()
            .filter(|e| e.kind == "artifact.registered")
            .map(|e| {
                (
                    e.data["path"].as_str().unwrap_or_default().to_owned(),
                    e.subject.step.map(|r| r.index),
                )
            })
            .collect()
    }

    /// `artifacts[]` names the step, whichever of the file or `commands.json` is seen first.
    #[test]
    fn a_failure_file_attaches_to_the_command_that_produced_it() {
        let mut s = session();
        file(&mut s, "pass/screenshots/step-004-odd.png", None);
        assert!(registered(&mut s).is_empty(), "waits for the step");
        file(&mut s, "pass/commands.json", Some(LISTED));
        assert_eq!(
            registered(&mut s),
            [("pass/screenshots/step-004-odd.png".to_owned(), Some(2))]
        );

        let mut s = session();
        file(&mut s, "pass/commands.json", Some(LISTED));
        file(&mut s, "pass/screenshots/step-004-odd.png", None);
        assert_eq!(
            registered(&mut s),
            [("pass/screenshots/step-004-odd.png".to_owned(), Some(2))]
        );
    }

    /// A file `artifacts[]` does not list is not guessed onto a step (I3).
    #[test]
    fn an_unlisted_file_gets_no_step() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(LISTED));
        file(&mut s, "pass/screenshots/step-003-other.png", None);
        assert_eq!(
            registered(&mut s),
            [("pass/screenshots/step-003-other.png".to_owned(), None)]
        );
    }

    /// A failure screenshot is queued for upload once its step is known, and carries that step.
    #[test]
    fn a_failure_screenshot_is_queued_for_upload_with_its_step() {
        let mut s = session();
        let flow = s.flows[0].id;
        file(&mut s, "pass/screenshots/step-004-odd.png", None);
        assert!(s.take_uploads().is_empty(), "waits for the step");
        file(&mut s, "pass/commands.json", Some(LISTED));
        assert_eq!(
            s.take_uploads(),
            [
                Candidate {
                    class: ArtifactClass::FailureScreenshot,
                    rel: "pass/screenshots/step-004-odd.png".into(),
                    flow: Some(flow),
                    step: Some(2),
                    failure: None,
                },
                Candidate {
                    class: ArtifactClass::Commands,
                    rel: "pass/commands.json".into(),
                    flow: Some(flow),
                    step: None,
                    failure: None,
                },
            ]
        );
    }

    /// On the captured `fail` flow, the failing step's screenshot and UI hierarchy are both queued
    /// for upload with that step (index 4), before the commands that name the step.
    #[test]
    fn a_failing_steps_hierarchy_is_queued_for_upload_with_its_step() {
        const FAIL: &[u8] =
            include_bytes!("../../../fixtures/maestro/2.12.1/fail/run/debug/fail/commands.json");
        let mut s = session();
        let flow = s.flows[0].id;
        file(
            &mut s,
            "pass/screenshots/step-005-assertCondition-Fleet_never_shows_this.png",
            None,
        );
        file(
            &mut s,
            "pass/screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json",
            None,
        );
        file(&mut s, "pass/commands.json", Some(FAIL));
        assert_eq!(
            s.take_uploads(),
            [
                Candidate {
                    class: ArtifactClass::FailureScreenshot,
                    rel: "pass/screenshots/step-005-assertCondition-Fleet_never_shows_this.png"
                        .into(),
                    flow: Some(flow),
                    step: Some(4),
                    failure: None,
                },
                Candidate {
                    class: ArtifactClass::FailureHierarchy,
                    rel:
                        "pass/screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json"
                            .into(),
                    flow: Some(flow),
                    step: Some(4),
                    failure: None,
                },
                Candidate {
                    class: ArtifactClass::DeviceLogcat,
                    rel: "pass/logs/device-logcat.txt".into(),
                    flow: Some(flow),
                    step: Some(4),
                    failure: Some(logcat::Failure {
                        start_ms: 1_793_751_912_442,
                        end_ms: 1_793_751_928_077,
                    }),
                },
                Candidate {
                    class: ArtifactClass::Commands,
                    rel: "pass/commands.json".into(),
                    flow: Some(flow),
                    step: None,
                    failure: None,
                },
            ]
        );
    }

    /// Older output without `artifacts[]` links a file to its step by name.
    #[test]
    fn older_output_links_a_file_by_its_name() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(GOOD));
        file(&mut s, "pass/screenshots/step-003-x.png", None);
        assert_eq!(
            registered(&mut s),
            [("pass/screenshots/step-003-x.png".to_owned(), Some(2))]
        );
    }

    /// A file still waiting when the run ends falls back to its name, and is journalled before the
    /// run ends.
    #[test]
    fn a_file_still_waiting_at_run_end_keeps_its_name_link() {
        let mut s = session();
        file(&mut s, "pass/screenshots/step-003-x.png", None);
        s.end(at(), 0);
        let events = s.drain();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds[kinds.len() - 2..],
            ["artifact.registered", "run.ended"]
        );
        assert_eq!(
            events[events.len() - 2]
                .subject
                .step
                .as_ref()
                .map(|r| r.index),
            Some(2)
        );
    }

    /// The `crash` fixture's failed assertion: `commands.json` gives its start and duration.
    const TIMED_FAILURE: &[u8] = br#"[{"metadata":{"status":"COMPLETED","sequenceNumber":0,"depth":0,"timestamp":1000,"duration":5}},
        {"metadata":{"status":"FAILED","sequenceNumber":1,"depth":0,"timestamp":2000,"duration":18287}}]"#;

    /// A failed flow queues its device log excerpt for the window of its failing command, and the
    /// excerpt is that command's step (issue #75). A passing flow queues none.
    #[test]
    fn a_failed_flow_queues_its_logcat_window_and_a_passing_one_does_not() {
        let mut s = session();
        let flow = s.flows[0].id;
        file(&mut s, "pass/commands.json", Some(TIMED_FAILURE));
        file(&mut s, "pass/manifest.json", None);
        let logcat: Vec<Candidate> = s
            .take_uploads()
            .into_iter()
            .filter(|c| c.class == ArtifactClass::DeviceLogcat)
            .collect();
        assert_eq!(
            logcat,
            [Candidate {
                class: ArtifactClass::DeviceLogcat,
                rel: "pass/logs/device-logcat.txt".into(),
                flow: Some(flow),
                step: Some(1),
                failure: Some(logcat::Failure {
                    start_ms: 2000,
                    end_ms: 20287,
                }),
            }]
        );

        let mut passing = session();
        file(&mut passing, "pass/commands.json", Some(GOOD));
        file(&mut passing, "pass/manifest.json", None);
        assert!(
            passing
                .take_uploads()
                .iter()
                .all(|c| c.class != ArtifactClass::DeviceLogcat)
        );
    }

    /// A failed command with no timing names no window: no excerpt, and a warning says why.
    #[test]
    fn a_failure_without_timing_keeps_no_excerpt_and_says_so() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(FAILED));
        assert!(
            s.take_uploads()
                .iter()
                .all(|c| c.class != ArtifactClass::DeviceLogcat)
        );
        assert!(warnings(&mut s).contains(&"logcat_window_unknown".to_owned()));
    }

    /// The app's crash in `logs/crash-report.txt` is journalled on its flow as `app.crashed`, and
    /// the flow's verdict stays Maestro's failed assertion.
    #[test]
    fn a_crash_report_is_journalled_on_its_flow_without_changing_the_verdict() {
        let mut s = session();
        let report = include_bytes!(
            "../../../fixtures/maestro/2.12.1/crash/run/debug/crash/logs/crash-report.txt"
        );
        file(&mut s, "pass/commands.json", Some(TIMED_FAILURE));
        file(&mut s, "pass/manifest.json", None);
        file(&mut s, "pass/logs/crash-report.txt", Some(report));
        let events = s.drain();
        let crashed: Vec<&EventEnvelope> =
            events.iter().filter(|e| e.kind == "app.crashed").collect();
        assert_eq!(crashed.len(), 1);
        assert_eq!(crashed[0].data["package"], "com.android.settings");
        assert_eq!(
            crashed[0].data["exception"],
            "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
        );
        assert_eq!(crashed[0].subject.flow_execution_id, Some(s.flows[0].id));
        assert_eq!(
            s.summary()[0].1.state,
            fleet_core::claims::FlowState::Ended(FlowOutcome::Failed)
        );
    }

    /// A flow with no crash report journals no crash.
    #[test]
    fn a_flow_without_a_crash_journals_none() {
        let mut s = session();
        file(&mut s, "pass/commands.json", Some(GOOD));
        file(&mut s, "pass/manifest.json", None);
        assert!(s.drain().iter().all(|e| e.kind != "app.crashed"));
    }
}
