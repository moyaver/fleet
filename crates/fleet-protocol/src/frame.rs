// SPDX-License-Identifier: AGPL-3.0-only
//! WebSocket frames on `fleet.v1+json` (research 05 §3.3–§3.5).
//!
//! The agent sends `hello`, then contiguous `events` batches; the server acks cumulatively after
//! committing (`ack{through}`). Resume is server-authoritative: `welcome.committed_through` tells
//! the agent what to drop and what to resend. Upload and throttle frames arrive with M5.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    event::AdbState,
    journal::{ClockSample, EventEnvelope},
};

/// Agent → server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Opens or resumes an agent run.
    Hello(Box<Hello>),
    /// Journal events, contiguous in `seq`.
    Events {
        /// The batch, in `seq` order.
        events: Vec<EventEnvelope>,
    },
    /// Metric samples, a separate stream with its own sequence (gaps allowed, research 05 §3.2).
    Telemetry(Telemetry),
    /// Liveness and queue depth.
    Heartbeat {
        /// Agent clock at send.
        clock: ClockSample,
        /// Events waiting for an ack.
        unacked: u64,
    },
    /// Graceful end, after `run.ended` was acked.
    Bye,
}

/// Server → agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The handshake succeeded.
    Welcome(Welcome),
    /// The handshake was refused.
    Reject {
        /// Why.
        code: ErrorCode,
        /// Detail.
        message: String,
        /// When to try again, if at all.
        retry_after_ms: Option<u64>,
        /// Don't retry with the same credentials or run.
        fatal: bool,
    },
    /// Everything up to and including `through` is committed.
    Ack {
        /// Highest committed `seq`.
        through: u64,
        /// The connection epoch the commit happened under.
        epoch: u32,
        /// Events in the batch that were already committed.
        #[serde(default)]
        duplicates: u32,
    },
    /// Telemetry up to and including `through` is stored.
    TelemetryAck {
        /// Highest stored `tseq`.
        through: u64,
    },
    /// One event was refused for good; the cursor moved past it. Don't resend it.
    EventRejected {
        /// Which event.
        seq: u64,
        /// Why.
        code: ErrorCode,
        /// Always true today.
        permanent: bool,
        /// Detail.
        message: String,
    },
    /// Something went wrong. `fatal` errors are followed by a close.
    Error {
        /// Why.
        code: ErrorCode,
        /// Whether the server will close.
        fatal: bool,
        /// Detail.
        message: String,
        /// For `seq_gap`: the `seq` the server expects next.
        expected_seq: Option<u64>,
    },
    /// The server is going away; reconnect after the given delay.
    Goaway {
        /// Why.
        reason: GoawayReason,
        /// Delay before reconnecting.
        reconnect_after_ms: u64,
    },
}

/// The HTTPS batch fallback (`POST /api/v1/agent/batch`) for networks that block web sockets:
/// one `hello`, then what a socket would have carried. Each request is a connection of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchRequest {
    /// Opens or resumes the run, exactly as on the socket.
    pub hello: Hello,
    /// Journal events, contiguous in `seq`.
    #[serde(default)]
    pub events: Vec<EventEnvelope>,
    /// Metric batches.
    #[serde(default)]
    pub telemetry: Vec<Telemetry>,
}

/// The frames the server would have sent on a socket, in order (`welcome`, `event_rejected`,
/// `ack`, `telemetry_ack`, `error`, or a lone `reject`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchResponse {
    /// The frames.
    pub frames: Vec<ServerFrame>,
}

/// A batch of metric samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Telemetry {
    /// First sample's `tseq`.
    pub tseq_from: u64,
    /// Last sample's `tseq`.
    pub tseq_to: u64,
    /// Samples dropped since the previous batch (the ring was full).
    #[serde(default)]
    pub dropped_since_last: u32,
    /// The samples.
    pub samples: Vec<Sample>,
    /// Device samples taken since the previous batch, one per device per sampling interval
    /// (PLAN §6.5). Absent from older agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceSample>,
}

/// One device's sample: what adb and the guest reported about a device session at one time.
/// `None` means that reading was not observed; a device that could not be reached has `None`
/// readings, never zeros.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceSample {
    /// When the probe was started.
    pub at: ClockSample,
    /// The device session the sample belongs to (its `shard.assigned` event names it).
    pub device_session_id: Uuid,
    /// adb's state for the device at sampling time, from the device list adb pushed. `Absent`
    /// means the list did not contain it.
    #[serde(default)]
    pub adb_state: Option<AdbState>,
    /// Whether `sys.boot_completed` was `1` when the probe ran. `None`: the probe did not answer.
    #[serde(default)]
    pub boot_completed: Option<bool>,
    /// The guest's CPU use since the previous probe of this device, percent of all its cores.
    /// `None` for the first probe and when the probe did not answer.
    #[serde(default)]
    pub guest_cpu_pct: Option<f32>,
    /// The guest's `MemAvailable`, bytes. `None` when the probe did not answer.
    #[serde(default)]
    pub guest_mem_available_bytes: Option<u64>,
    /// The emulator process's CPU use since the previous sample, percent of one core. `None` for
    /// a physical device, when no emulator process maps to the serial, or when it was unreadable.
    #[serde(default)]
    pub emu_cpu_pct: Option<f32>,
    /// The emulator process's resident memory, bytes. `None` as for `emu_cpu_pct`.
    #[serde(default)]
    pub emu_rss_bytes: Option<u64>,
    /// The emulator's AVD name, from its command line (`-avd`). `None` for a physical device.
    #[serde(default)]
    pub avd_name: Option<String>,
    /// The device's API level (`ro.build.version.sdk`), once read. `None` until then.
    #[serde(default)]
    pub api_level: Option<u32>,
    /// The component of the resumed activity. Set only on the sample taken after a flow failed on
    /// this device (PLAN §6.5: current activity only on failure); `None` otherwise.
    #[serde(default)]
    pub foreground_activity: Option<String>,
    /// The shard the device was assigned to: the planner's shard for a plan run (`plan.assigned`,
    /// as the plan numbers it), otherwise the `shard_number` its `shard.assigned` event names
    /// (1-based). Absent from older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<u32>,
    /// Why each reading above is unknown, for every reading that is `None` (PLAN §6.5: availability
    /// with a reason when unknown). Empty from older agents.
    #[serde(default, skip_serializing_if = "DeviceUnavailable::is_empty")]
    pub unavailable: DeviceUnavailable,
    /// The device's model (`ro.product.model`), once read; carried on every sample after that.
    /// Absent from older agents (issue #85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The device's OS version (`ro.build.version.release`), once read, as `model`. Absent from
    /// older agents (issue #85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
}

/// Why a device, host or process reading is unknown (PLAN §6.5). A reading that is set has no
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    /// The serial is not an emulator's (`emulator-<console port>`), so no emulator process is read.
    NotAnEmulator,
    /// An emulator serial that no `qemu-system-*` process maps to (it exited, or the process table
    /// could not be read), or a device session the agent no longer drives.
    NoEmulatorProcess,
    /// The emulator's command line names no AVD (`-avd` is optional).
    NoAvdName,
    /// adb did not answer the read, or answered without it (an error, a timeout or a malformed
    /// reply).
    ProbeFailed,
    /// The host is under pressure, so the guest's CPU and memory are not read (PLAN §6.5).
    Throttled,
    /// The first probe of a device only sets its CPU baseline; so does one after the guest's
    /// counters went backwards.
    NoBaseline,
    /// adb's device list has not arrived yet.
    NoAdbList,
    /// This sample does not take the reading: a probe sample carries no activity, and the sample
    /// of a failed flow's activity carries no probe.
    NotSampled,
    /// The platform's system information could not be read, so no host or process reading that
    /// comes from it was taken.
    NoSystemInfo,
    /// The kernel has no pressure stall information (PSI) for the resource: macOS and Windows.
    NoPsi,
    /// The process is not known to this run yet (Maestro has not started), or it has exited.
    NoProcess,
    /// A reason this build doesn't know.
    #[serde(other)]
    Other,
}

/// The reason each device reading is unknown. Each field is the reason for the reading of the
/// same name in [`DeviceSample`]; a reading that is set has `None` here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceUnavailable {
    /// Why `adb_state` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_state: Option<Unavailable>,
    /// Why `boot_completed` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_completed: Option<Unavailable>,
    /// Why `guest_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_cpu_pct: Option<Unavailable>,
    /// Why `guest_mem_available_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_mem_available_bytes: Option<Unavailable>,
    /// Why `emu_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emu_cpu_pct: Option<Unavailable>,
    /// Why `emu_rss_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emu_rss_bytes: Option<Unavailable>,
    /// Why `avd_name` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avd_name: Option<Unavailable>,
    /// Why `api_level` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_level: Option<Unavailable>,
    /// Why `foreground_activity` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_activity: Option<Unavailable>,
}

impl DeviceUnavailable {
    /// Whether no reading has a reason, so nothing needs sending.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.adb_state.is_none()
            && self.boot_completed.is_none()
            && self.guest_cpu_pct.is_none()
            && self.guest_mem_available_bytes.is_none()
            && self.emu_cpu_pct.is_none()
            && self.emu_rss_bytes.is_none()
            && self.avd_name.is_none()
            && self.api_level.is_none()
            && self.foreground_activity.is_none()
    }
}

/// The reason each reading of a [`Sample`] is unknown. Each field is the reason for the reading of
/// the same name; a reading that is set has `None` here (PLAN §6.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SampleUnavailable {
    /// Why `host_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_cpu_pct: Option<Unavailable>,
    /// Why `host_mem_used_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_mem_used_bytes: Option<Unavailable>,
    /// Why `host_mem_total_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_mem_total_bytes: Option<Unavailable>,
    /// Why `maestro_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maestro_cpu_pct: Option<Unavailable>,
    /// Why `maestro_rss_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maestro_rss_bytes: Option<Unavailable>,
    /// Why `agent_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cpu_pct: Option<Unavailable>,
    /// Why `agent_rss_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_rss_bytes: Option<Unavailable>,
    /// Why `host_load_1m` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_load_1m: Option<Unavailable>,
    /// Why `host_cpu_cores` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_cpu_cores: Option<Unavailable>,
    /// Why `host_swap_used_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_swap_used_bytes: Option<Unavailable>,
    /// Why `host_swap_total_bytes` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_swap_total_bytes: Option<Unavailable>,
    /// Why `host_psi_cpu_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_psi_cpu_pct: Option<Unavailable>,
    /// Why `host_psi_memory_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_psi_memory_pct: Option<Unavailable>,
    /// Why `host_psi_io_pct` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_psi_io_pct: Option<Unavailable>,
}

impl SampleUnavailable {
    /// Whether no reading has a reason, so nothing needs sending.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.host_cpu_pct.is_none()
            && self.host_mem_used_bytes.is_none()
            && self.host_mem_total_bytes.is_none()
            && self.maestro_cpu_pct.is_none()
            && self.maestro_rss_bytes.is_none()
            && self.agent_cpu_pct.is_none()
            && self.agent_rss_bytes.is_none()
            && self.host_load_1m.is_none()
            && self.host_cpu_cores.is_none()
            && self.host_swap_used_bytes.is_none()
            && self.host_swap_total_bytes.is_none()
            && self.host_psi_cpu_pct.is_none()
            && self.host_psi_memory_pct.is_none()
            && self.host_psi_io_pct.is_none()
    }
}

/// One metric sample. `None` means unavailable, never zero (research 05 §5.4).
///
/// Every reading carries its unit in its field name (`_pct` percent, `_bytes` bytes, `_cores`
/// cores; the load average is a plain number), and its scope in its prefix: `host_` is the
/// machine, `maestro_` the Maestro process of the run and `agent_` the agent itself (its
/// supervisor and observer). A reading that is `None` has its reason in `unavailable`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Sample {
    /// When it was taken.
    pub at: ClockSample,
    /// Host CPU use across all cores, percent.
    pub host_cpu_pct: Option<f32>,
    /// Host memory in use, bytes.
    pub host_mem_used_bytes: Option<u64>,
    /// Host memory total, bytes.
    pub host_mem_total_bytes: Option<u64>,
    /// Maestro's JVM: CPU percent of one core.
    pub maestro_cpu_pct: Option<f32>,
    /// Maestro's JVM: resident memory, bytes.
    pub maestro_rss_bytes: Option<u64>,
    /// The agent itself (supervisor + observer): CPU percent of one core since the last sample
    /// (invariant I10).
    pub agent_cpu_pct: Option<f32>,
    /// The agent itself (supervisor + observer): current resident memory of both, summed, bytes.
    pub agent_rss_bytes: Option<u64>,
    /// Host load average over one minute (runnable tasks, averaged). Absent from older agents.
    #[serde(default)]
    pub host_load_1m: Option<f32>,
    /// Host physical CPU cores. Absent from older agents.
    #[serde(default)]
    pub host_cpu_cores: Option<u32>,
    /// Host swap in use, bytes. Absent from older agents.
    #[serde(default)]
    pub host_swap_used_bytes: Option<u64>,
    /// Host swap configured, bytes. Absent from older agents.
    #[serde(default)]
    pub host_swap_total_bytes: Option<u64>,
    /// Linux pressure stall information: percent of the last 10 seconds in which some task
    /// waited for CPU. `None` where the kernel has no PSI (macOS, Windows). Absent from older
    /// agents.
    #[serde(default)]
    pub host_psi_cpu_pct: Option<f32>,
    /// Linux pressure stall information: percent of the last 10 seconds in which some task
    /// waited for memory. Absent from older agents.
    #[serde(default)]
    pub host_psi_memory_pct: Option<f32>,
    /// Linux pressure stall information: percent of the last 10 seconds in which some task
    /// waited for I/O. Absent from older agents.
    #[serde(default)]
    pub host_psi_io_pct: Option<f32>,
    /// Why each reading above is unknown, for every reading that is `None` (PLAN §6.5). Empty from
    /// older agents.
    #[serde(default, skip_serializing_if = "SampleUnavailable::is_empty")]
    pub unavailable: SampleUnavailable,
}

/// Where a run's evidence came from (issue #83). A live run is observed as it happens; a published
/// run is a finished run the agent sent after the fact (`fleet-agent publish`), so the server shows
/// it as imported, never as live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    /// Observed while it ran.
    #[default]
    Live,
    /// Sent after the fact by `fleet-agent publish`: an offline bundle or a saved output.
    Published,
}

impl RunOrigin {
    /// The stored and wire name: `live` or `published`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Published => "published",
        }
    }

    /// The origin named by [`Self::as_str`], or `None` for any other text.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "live" => Some(Self::Live),
            "published" => Some(Self::Published),
            _ => None,
        }
    }

    /// Whether this is [`RunOrigin::Live`] (the default, which older agents never send).
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

/// The handshake payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Hello {
    /// Protocol versions the agent speaks.
    pub protocol: ProtocolRange,
    /// The agent build.
    pub agent: AgentInfo,
    /// Generated once per agent process (UUIDv7); owns the `seq` space.
    pub agent_run_id: Uuid,
    /// The run this one replaces after an agent restart in the same job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_agent_run_id: Option<Uuid>,
    /// CI identity as the agent sees it; the server prefers OIDC claims.
    pub ci: CiContext,
    /// What the agent still holds.
    pub resume: ResumeState,
    /// Agent clock at send.
    pub clock: ClockSample,
    /// Where the run's evidence came from. Absent from live agents, which send nothing here.
    #[serde(default, skip_serializing_if = "RunOrigin::is_live")]
    pub origin: RunOrigin,
    /// When the run started, as the agent recorded it. A published run takes its date (`run_day`)
    /// from this, not from the day it is published; a live run leaves it out and starts now.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::rfc3339_option")
    )]
    pub started_at: Option<time::OffsetDateTime>,
    /// The flows the run's workspace holds, as the agent found them before Maestro ran (issue
    /// #85). Empty from older agents and when there is no workspace to read; the server then learns
    /// no flow it hasn't seen run.
    #[serde(default, skip_serializing_if = "FlowCatalog::is_empty")]
    pub catalog: FlowCatalog,
}

/// The flows a workspace holds, as the agent found them (issue #85). Bounded by the agent: at most
/// [`MAX_CATALOG_FLOWS`] entries, and `truncated` says more were found and left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FlowCatalog {
    /// The flows, sorted by path.
    #[serde(default)]
    pub flows: Vec<CatalogFlow>,
    /// More flows than `flows` lists were found.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl FlowCatalog {
    /// Whether the catalog says nothing: no flows and nothing left out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.flows.is_empty() && !self.truncated
    }
}

/// The most flows a catalog carries (issue #85). The agent stops at this many; the server refuses
/// nothing below it.
pub const MAX_CATALOG_FLOWS: usize = 500;
/// The longest path a catalog flow may have, in bytes.
pub const MAX_CATALOG_PATH_BYTES: usize = 512;
/// The most tags a catalog flow may carry.
pub const MAX_CATALOG_TAGS: usize = 16;
/// The longest tag, in bytes.
pub const MAX_CATALOG_TAG_BYTES: usize = 64;

/// One flow file of the workspace, as its header describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogFlow {
    /// The flow's path, relative to the repository root, as the flow's runs name it.
    pub path: String,
    /// The `name:` of its header, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `tags:` of its header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A closed range of protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProtocolRange {
    /// Oldest supported.
    pub min: u16,
    /// Newest supported.
    pub max: u16,
}

/// The agent build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentInfo {
    /// Agent version.
    pub version: String,
    /// `std::env::consts::OS`.
    pub os: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
}

/// What the agent can resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResumeState {
    /// The next `seq` the agent will assign.
    pub next_seq: u64,
    /// The oldest event it still holds unacked.
    pub oldest_unacked_seq: Option<u64>,
}

/// CI provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CiProvider {
    /// GitHub Actions.
    GithubActions,
    /// Anything else, described by `FLEET_*` variables.
    Generic,
}

/// CI identity as claimed by the agent (research 06 §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CiContext {
    /// Which CI.
    pub provider: CiProvider,
    /// `owner/name`.
    pub repository: Option<String>,
    /// Workflow run ID.
    pub run_id: Option<String>,
    /// Re-run attempt, from 1.
    pub run_attempt: Option<u32>,
    /// Stable job identity within the attempt (PLAN §6.7).
    pub job_key: String,
    /// Job display name.
    pub job_name: Option<String>,
    /// Commit SHA.
    pub sha: Option<String>,
    /// Git ref.
    pub git_ref: Option<String>,
    /// Runner name.
    pub runner: Option<String>,
}

/// The server's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Welcome {
    /// The version chosen.
    pub protocol: u16,
    /// This connection.
    pub connection_id: Uuid,
    /// Fencing epoch: older connections for the same run are superseded.
    pub epoch: u32,
    /// Highest `seq` the server has committed for this run.
    pub committed_through: u64,
    /// Whether the run is new, resumed or reopened after the grace period.
    pub resume: ResumeOutcome,
    /// Flow-control limits.
    pub limits: Limits,
}

/// Outcome of the resume negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResumeOutcome {
    /// The server had never seen this run.
    New,
    /// The run continues.
    Resumed,
    /// The run had been abandoned and is open again.
    Reopened,
}

/// Flow-control limits from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Limits {
    /// Events in flight (sent, not acked) at most.
    pub max_inflight_events: u32,
    /// Largest frame accepted.
    pub max_frame_bytes: u32,
    /// How often the agent sends a heartbeat.
    pub heartbeat_interval_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_inflight_events: 512,
            max_frame_bytes: 256 * 1024,
            heartbeat_interval_ms: 10_000,
        }
    }
}

/// Machine-readable error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Credentials missing or invalid.
    Unauthorized,
    /// No protocol version in common.
    UnsupportedProtocol,
    /// The agent reused a run ID with an older sequence.
    SeqRegression,
    /// A batch skipped sequence numbers.
    SeqGap,
    /// A newer connection took over this run.
    Superseded,
    /// The event broke a validation rule.
    Invalid,
    /// The frame was too large.
    TooLarge,
    /// Overloaded; retry later.
    Overloaded,
    /// Anything else.
    Internal,
    /// A code this build doesn't know.
    #[serde(other)]
    Unknown,
}

/// Why the server sent `goaway`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GoawayReason {
    /// The server is restarting.
    Shutdown,
    /// Load balancing.
    Rebalance,
    /// The agent's token expires soon.
    CredentialExpiring,
    /// A reason this build doesn't know.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_use_an_op_tag() {
        let ack = ServerFrame::Ack {
            through: 211,
            epoch: 7,
            duplicates: 0,
        };
        assert_eq!(serde_json::to_value(&ack).unwrap()["op"], "ack");
        let back: ServerFrame =
            serde_json::from_str(r#"{"op":"ack","through":5,"epoch":1}"#).unwrap();
        assert_eq!(
            back,
            ServerFrame::Ack {
                through: 5,
                epoch: 1,
                duplicates: 0
            },
            "duplicates defaults to 0"
        );
        assert_eq!(
            serde_json::to_value(ClientFrame::Bye).unwrap(),
            serde_json::json!({ "op": "bye" })
        );
    }

    #[test]
    fn sample_host_fields_added_later_are_optional() {
        // A sample from an agent that predates load, cores, swap and PSI.
        let old: Sample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},"host_cpu_pct":12.5,
                "host_mem_used_bytes":1,"host_mem_total_bytes":2,"maestro_cpu_pct":null,
                "maestro_rss_bytes":null,"agent_cpu_pct":null,"agent_rss_bytes":null}"#,
        )
        .unwrap();
        assert_eq!(
            (
                old.host_load_1m,
                old.host_cpu_cores,
                old.host_swap_used_bytes,
                old.host_psi_cpu_pct
            ),
            (None, None, None, None),
            "a missing field is unavailable, not zero"
        );
        let full = Sample {
            host_load_1m: Some(1.5),
            host_cpu_cores: Some(8),
            host_swap_used_bytes: Some(1 << 30),
            host_swap_total_bytes: Some(2 << 30),
            host_psi_cpu_pct: Some(0.25),
            host_psi_memory_pct: None,
            host_psi_io_pct: Some(3.0),
            ..old
        };
        let back: Sample = serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn sample_readings_say_why_they_are_unknown() {
        // A sample from an agent that predates reasons: none is invented, and none is sent.
        let old: Sample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},"host_cpu_pct":12.5,
                "host_mem_used_bytes":1,"host_mem_total_bytes":2,"maestro_cpu_pct":null,
                "maestro_rss_bytes":null,"agent_cpu_pct":null,"agent_rss_bytes":null}"#,
        )
        .unwrap();
        assert!(old.unavailable.is_empty());
        assert!(
            !serde_json::to_string(&old).unwrap().contains("unavailable"),
            "nothing unset is sent"
        );

        // macOS has no PSI: each pressure reading says so, and the Maestro process readings say the
        // process is not known yet. Absent readings stay absent, never zero.
        let mac = Sample {
            host_psi_cpu_pct: None,
            host_psi_memory_pct: None,
            host_psi_io_pct: None,
            unavailable: SampleUnavailable {
                host_psi_cpu_pct: Some(Unavailable::NoPsi),
                host_psi_memory_pct: Some(Unavailable::NoPsi),
                host_psi_io_pct: Some(Unavailable::NoPsi),
                maestro_cpu_pct: Some(Unavailable::NoProcess),
                maestro_rss_bytes: Some(Unavailable::NoProcess),
                ..SampleUnavailable::default()
            },
            ..old
        };
        let wire = serde_json::to_value(&mac).unwrap();
        assert_eq!(wire["unavailable"]["host_psi_cpu_pct"], "no_psi");
        assert_eq!(wire["unavailable"]["maestro_rss_bytes"], "no_process");
        assert!(
            wire["unavailable"].get("host_cpu_pct").is_none(),
            "a reading that has a value has no reason"
        );
        let back: Sample = serde_json::from_value(wire).unwrap();
        assert_eq!(back, mac);

        // A reason this build doesn't know decodes as `Other`, not as a failed frame.
        let newer: Sample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},
                "unavailable":{"host_load_1m":"brand_new_reason"}}"#,
        )
        .unwrap();
        assert_eq!(newer.unavailable.host_load_1m, Some(Unavailable::Other));
    }

    #[test]
    fn device_samples_are_optional_and_unknown_readings_stay_unknown() {
        // A telemetry frame from an agent that predates device samples.
        let old: Telemetry =
            serde_json::from_str(r#"{"tseq_from":1,"tseq_to":1,"samples":[]}"#).unwrap();
        assert!(old.devices.is_empty(), "absent means no device samples");
        assert!(
            !serde_json::to_string(&old).unwrap().contains("devices"),
            "an empty list is not sent, so older servers see the frame they know"
        );
        // A sample the probe could not complete: only what adb reported is set.
        let unreachable: DeviceSample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},
                "device_session_id":"00000000-0000-7000-8000-000000000001",
                "adb_state":"offline"}"#,
        )
        .unwrap();
        assert_eq!(
            (
                unreachable.adb_state,
                unreachable.boot_completed,
                unreachable.guest_cpu_pct,
                unreachable.guest_mem_available_bytes
            ),
            (Some(AdbState::Offline), None, None, None),
            "a missing reading is unknown, not zero"
        );
        let full = Telemetry {
            devices: vec![unreachable],
            ..old
        };
        let back: Telemetry = serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn device_samples_carry_their_shard_and_a_reason_for_each_unknown_reading() {
        // An agent that predates shards and reasons: both are absent, and nothing is invented.
        let old: DeviceSample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},
                "device_session_id":"00000000-0000-7000-8000-000000000001"}"#,
        )
        .unwrap();
        assert_eq!(
            (old.shard, old.unavailable),
            (None, DeviceUnavailable::default())
        );
        assert!(old.unavailable.is_empty());
        let wire = serde_json::to_string(&old).unwrap();
        assert!(
            !wire.contains("shard") && !wire.contains("unavailable"),
            "nothing unset is sent: {wire}"
        );

        let throttled = DeviceSample {
            shard: Some(2),
            unavailable: DeviceUnavailable {
                guest_cpu_pct: Some(Unavailable::Throttled),
                guest_mem_available_bytes: Some(Unavailable::Throttled),
                emu_cpu_pct: Some(Unavailable::NotAnEmulator),
                avd_name: Some(Unavailable::NotAnEmulator),
                ..DeviceUnavailable::default()
            },
            ..old
        };
        let wire = serde_json::to_value(&throttled).unwrap();
        assert_eq!(wire["shard"], 2);
        assert_eq!(wire["unavailable"]["guest_cpu_pct"], "throttled");
        assert_eq!(wire["unavailable"]["emu_cpu_pct"], "not_an_emulator");
        assert!(
            wire["unavailable"].get("boot_completed").is_none(),
            "a reading that has no reason is not listed"
        );
        let back: DeviceSample = serde_json::from_value(wire).unwrap();
        assert_eq!(back, throttled);

        // A reason this build doesn't know decodes as `Other`, not as a failed frame.
        let newer: DeviceSample = serde_json::from_str(
            r#"{"at":{"wall":"2026-01-01T00:00:00Z","mono_ms":0},
                "device_session_id":"00000000-0000-7000-8000-000000000001",
                "unavailable":{"boot_completed":"brand_new_reason"}}"#,
        )
        .unwrap();
        assert_eq!(newer.unavailable.boot_completed, Some(Unavailable::Other));
    }

    #[test]
    fn unknown_codes_decode_instead_of_failing() {
        let f: ServerFrame = serde_json::from_str(
            r#"{"op":"error","code":"brand_new","fatal":false,"message":"x","expected_seq":null}"#,
        )
        .unwrap();
        assert!(matches!(
            f,
            ServerFrame::Error {
                code: ErrorCode::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn a_hello_without_a_catalog_is_an_older_agents_and_an_empty_catalog_is_not_sent() {
        let mut hello = Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            agent: AgentInfo {
                version: "0.0.0".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            },
            agent_run_id: Uuid::nil(),
            previous_agent_run_id: None,
            ci: CiContext {
                provider: CiProvider::Generic,
                repository: None,
                run_id: None,
                run_attempt: None,
                job_key: "job".into(),
                job_name: None,
                sha: None,
                git_ref: None,
                runner: None,
            },
            resume: ResumeState {
                next_seq: 1,
                oldest_unacked_seq: None,
            },
            clock: ClockSample {
                wall: time::OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            },
            origin: RunOrigin::Live,
            started_at: None,
            catalog: FlowCatalog::default(),
        };
        let json = serde_json::to_value(&hello).unwrap();
        assert!(
            json.get("catalog").is_none(),
            "an empty catalog is left out: {json}"
        );
        // An older agent's hello, which has no catalog, decodes with an empty one.
        let decoded: Hello = serde_json::from_value(json).unwrap();
        assert!(decoded.catalog.is_empty());
        hello.catalog = FlowCatalog {
            flows: vec![CatalogFlow {
                path: "flows/login.yaml".into(),
                name: Some("login".into()),
                tags: vec!["smoke".into()],
            }],
            truncated: true,
        };
        let json = serde_json::to_value(&hello).unwrap();
        let back: Hello = serde_json::from_value(json).unwrap();
        assert_eq!(back.catalog, hello.catalog);
    }

    #[test]
    fn a_device_sample_without_identity_fields_decodes_from_an_older_agent() {
        let sample = DeviceSample {
            model: Some("Pixel_8".into()),
            os_version: None,
            at: ClockSample {
                wall: time::OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            },
            device_session_id: Uuid::nil(),
            adb_state: None,
            boot_completed: None,
            guest_cpu_pct: None,
            guest_mem_available_bytes: None,
            emu_cpu_pct: None,
            emu_rss_bytes: None,
            avd_name: None,
            api_level: None,
            foreground_activity: None,
            shard: None,
            unavailable: DeviceUnavailable::default(),
        };
        let json = serde_json::to_value(&sample).unwrap();
        assert_eq!(json.get("model"), Some(&serde_json::json!("Pixel_8")));
        assert!(json.get("os_version").is_none());
        let mut older = json.clone();
        older.as_object_mut().unwrap().remove("model");
        let decoded: DeviceSample = serde_json::from_value(older).unwrap();
        assert_eq!(decoded.model, None);
    }
}
