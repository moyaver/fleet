// SPDX-License-Identifier: AGPL-3.0-only
//! Host, Maestro and agent metrics (PLAN §6.5, research 02 §2; invariant I10).
//!
//! Only targeted refreshes: host CPU and memory, and one process (Maestro's JVM). The agent's own
//! cost, supervisor plus observer, is a reported number, not a guess: the observer's CPU comes from
//! `getrusage`, the supervisor's from a targeted refresh of its one PID, and resident memory of
//! both from the same refresh.

use std::time::{Duration, Instant};

use std::path::Path;

use fleet_protocol::{
    frame::{Sample, SampleUnavailable, Unavailable},
    journal::ClockSample,
};
use nix::sys::resource::{UsageWho, getrusage};
use sysinfo::{LoadAvg, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Samples metrics; keep one per run so CPU percentages have a baseline.
pub(crate) struct Sampler {
    /// `None` when the platform's system information is unavailable: samples say so.
    system: Option<System>,
    maestro: Option<Pid>,
    /// The supervisor process, passed explicitly by the supervisor (never derived from `getppid`).
    supervisor: Option<Pid>,
    /// Baseline of the observer's own CPU time.
    last_self: Option<(Instant, Duration)>,
    /// Baseline of the supervisor's accumulated CPU time.
    last_supervisor: Option<Duration>,
    /// Physical CPU cores, read once: it does not change while the agent runs.
    cpu_cores: Option<u32>,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("maestro", &self.maestro)
            .field("supervisor", &self.supervisor)
            .finish_non_exhaustive()
    }
}

impl Sampler {
    /// A sampler watching Maestro and, for the agent's own cost, the supervisor process.
    pub(crate) fn new(maestro_pid: Option<u32>, supervisor_pid: Option<u32>) -> Self {
        let system = System::new_with_specifics(RefreshKind::nothing())
            .ok()
            .map(|mut s| {
                s.refresh_cpu_usage();
                s.refresh_memory();
                s
            });
        Self {
            system,
            maestro: maestro_pid.map(Pid::from_u32),
            supervisor: supervisor_pid.map(Pid::from_u32),
            last_self: None,
            last_supervisor: None,
            cpu_cores: System::physical_core_count()
                .ok()
                .and_then(|n| u32::try_from(n).ok()),
        }
    }

    /// One sample. Every reading that is unknown carries its reason (PLAN §6.5): a platform without
    /// system information, a process this run does not know (or that exited), a probe that failed,
    /// a first CPU sample with no baseline, or a kernel without pressure information.
    pub(crate) fn sample(&mut self, at: ClockSample) -> Sample {
        self.sample_at(at, Instant::now())
    }

    /// [`sample`](Self::sample) with the instant that ends the CPU window, so a test sets the
    /// window instead of measuring a wall-clock one.
    fn sample_at(&mut self, at: ClockSample, now: Instant) -> Sample {
        let (agent_cpu, agent_rss) = self.agent_usage(now);
        let (agent_cpu_pct, agent_cpu_why) = split(agent_cpu);
        let (agent_rss_bytes, agent_rss_why) = split(agent_rss);
        // The core count is read once at start, not from the system information.
        let (host_cpu_cores, cores_why) = split(self.cpu_cores.ok_or(Unavailable::ProbeFailed));
        let Some(system) = self.system.as_mut() else {
            return Sample {
                at,
                host_cpu_pct: None,
                host_mem_used_bytes: None,
                host_mem_total_bytes: None,
                runner_cpu_pct: None,
                runner_rss_bytes: None,
                agent_cpu_pct,
                agent_rss_bytes,
                host_load_1m: None,
                host_cpu_cores,
                host_swap_used_bytes: None,
                host_swap_total_bytes: None,
                host_psi_cpu_pct: None,
                host_psi_memory_pct: None,
                host_psi_io_pct: None,
                unavailable: SampleUnavailable {
                    host_cpu_pct: Some(Unavailable::NoSystemInfo),
                    host_mem_used_bytes: Some(Unavailable::NoSystemInfo),
                    host_mem_total_bytes: Some(Unavailable::NoSystemInfo),
                    runner_cpu_pct: Some(Unavailable::NoSystemInfo),
                    runner_rss_bytes: Some(Unavailable::NoSystemInfo),
                    agent_cpu_pct: agent_cpu_why,
                    agent_rss_bytes: agent_rss_why,
                    host_load_1m: Some(Unavailable::NoSystemInfo),
                    host_cpu_cores: cores_why,
                    host_swap_used_bytes: Some(Unavailable::NoSystemInfo),
                    host_swap_total_bytes: Some(Unavailable::NoSystemInfo),
                    host_psi_cpu_pct: Some(Unavailable::NoSystemInfo),
                    host_psi_memory_pct: Some(Unavailable::NoSystemInfo),
                    host_psi_io_pct: Some(Unavailable::NoSystemInfo),
                },
            };
        };
        system.refresh_cpu_usage();
        system.refresh_memory();
        let maestro = match self.maestro {
            Some(pid) => {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_cpu().with_memory(),
                );
                system
                    .process(pid)
                    .map(|p| (p.cpu_usage(), p.memory()))
                    .ok_or(Unavailable::NoProcess)
            }
            None => Err(Unavailable::NoProcess),
        };
        let total = system.total_memory();
        let (runner_cpu_pct, maestro_cpu_why) = split(maestro.map(|(cpu, _)| cpu));
        let (runner_rss_bytes, maestro_rss_why) = split(maestro.map(|(_, rss)| rss));
        let (host_mem_used_bytes, mem_used_why) = split(nonzero(total, system.used_memory()));
        let (host_mem_total_bytes, mem_total_why) = split(nonzero(total, total));
        let (host_load_1m, load_why) = split(
            System::load_average()
                .map(|load| load_one(&load))
                .map_err(|_| Unavailable::ProbeFailed),
        );
        let (host_psi_cpu_pct, psi_cpu_why) = split(read_psi("cpu"));
        let (host_psi_memory_pct, psi_memory_why) = split(read_psi("memory"));
        let (host_psi_io_pct, psi_io_why) = split(read_psi("io"));
        Sample {
            at,
            host_cpu_pct: Some(system.global_cpu_usage()),
            host_mem_used_bytes,
            host_mem_total_bytes,
            runner_cpu_pct,
            runner_rss_bytes,
            agent_cpu_pct,
            agent_rss_bytes,
            host_load_1m,
            host_cpu_cores,
            host_swap_used_bytes: Some(system.used_swap()),
            host_swap_total_bytes: Some(system.total_swap()),
            host_psi_cpu_pct,
            host_psi_memory_pct,
            host_psi_io_pct,
            unavailable: SampleUnavailable {
                host_mem_used_bytes: mem_used_why,
                host_mem_total_bytes: mem_total_why,
                runner_cpu_pct: maestro_cpu_why,
                runner_rss_bytes: maestro_rss_why,
                agent_cpu_pct: agent_cpu_why,
                agent_rss_bytes: agent_rss_why,
                host_load_1m: load_why,
                host_cpu_cores: cores_why,
                host_psi_cpu_pct: psi_cpu_why,
                host_psi_memory_pct: psi_memory_why,
                host_psi_io_pct: psi_io_why,
                ..SampleUnavailable::default()
            },
        }
    }

    /// The agent's cost (observer plus supervisor): CPU since the previous sample as a percentage
    /// of one core (sum of both processes' CPU-time deltas, each against its own baseline), and
    /// the current resident memory of both. When the supervisor is gone or unknown, the numbers
    /// cover the observer only. Each is either the value or its reason.
    fn agent_usage(
        &mut self,
        now: Instant,
    ) -> (Result<f32, Unavailable>, Result<u64, Unavailable>) {
        let own = getrusage(UsageWho::RUSAGE_SELF)
            .ok()
            .map(|u| tv(u.user_time()) + tv(u.system_time()));
        let mut rss = Err(Unavailable::NoSystemInfo);
        let mut supervisor_cpu = None;
        if let Some(system) = self.system.as_mut() {
            let own_pid = Pid::from_u32(std::process::id());
            let mut pids = vec![own_pid];
            pids.extend(self.supervisor);
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            let own_rss = system.process(own_pid).map(sysinfo::Process::memory);
            let sup = self.supervisor.and_then(|p| system.process(p));
            supervisor_cpu = sup.map(|p| Duration::from_millis(p.accumulated_cpu_time()));
            rss = own_rss
                .map(|r| r + sup.map_or(0, sysinfo::Process::memory))
                .ok_or(Unavailable::ProbeFailed);
        }
        let sup_delta = match (supervisor_cpu, self.last_supervisor) {
            (Some(cpu), Some(before)) => cpu.saturating_sub(before),
            _ => Duration::ZERO,
        };
        self.last_supervisor = supervisor_cpu.or(self.last_supervisor);
        let pct = match (own, self.last_self) {
            (Some(cpu), Some((then, before))) => {
                let wall = now.duration_since(then).as_secs_f64();
                let busy = cpu.saturating_sub(before) + sup_delta;
                #[expect(clippy::cast_possible_truncation, reason = "a percentage fits in f32")]
                let p = if wall > 0.0 {
                    (busy.as_secs_f64() / wall * 100.0) as f32
                } else {
                    0.0
                };
                Ok(p)
            }
            (Some(_), None) => Err(Unavailable::NoBaseline),
            (None, _) => Err(Unavailable::ProbeFailed),
        };
        if let Some(cpu) = own {
            self.last_self = Some((now, cpu));
        }
        (pct, rss)
    }
}

/// A reading's value, and the reason it is unknown when there is none.
fn split<T>(reading: Result<T, Unavailable>) -> (Option<T>, Option<Unavailable>) {
    match reading {
        Ok(value) => (Some(value), None),
        Err(why) => (None, Some(why)),
    }
}

/// `value` when the system reports a total, else a failed probe.
fn nonzero<T>(total: u64, value: T) -> Result<T, Unavailable> {
    if total > 0 {
        Ok(value)
    } else {
        Err(Unavailable::ProbeFailed)
    }
}

/// The one-minute load average as the sample carries it.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a load average is a small number; f32 keeps far more digits than it needs"
)]
fn load_one(load: &LoadAvg) -> f32 {
    load.one as f32
}

/// Linux pressure stall information for `resource` (`cpu`, `memory` or `io`): the `some` share of
/// the last ten seconds, or why there is none.
fn read_psi(resource: &str) -> Result<f32, Unavailable> {
    read_psi_file(Path::new(&format!("/proc/pressure/{resource}")))
}

/// [`read_psi`] for one file. A missing file means the kernel has no PSI ([`Unavailable::NoPsi`],
/// as on macOS); a file that cannot be read or parsed is a failed probe.
fn read_psi_file(path: &Path) -> Result<f32, Unavailable> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_psi_some_avg10(&text).ok_or(Unavailable::ProbeFailed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Unavailable::NoPsi),
        Err(_) => Err(Unavailable::ProbeFailed),
    }
}

/// The `avg10` figure of the `some` line of a PSI file, for example
/// `some avg10=0.12 avg60=0.04 avg300=0.01 total=1234`. The `full` line is not taken for it.
fn parse_psi_some_avg10(text: &str) -> Option<f32> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

fn tv(t: nix::sys::time::TimeVal) -> Duration {
    let secs = u64::try_from(t.tv_sec()).unwrap_or(0);
    let micros = u64::try_from(t.tv_usec()).unwrap_or(0);
    Duration::from_secs(secs) + Duration::from_micros(micros)
}

/// The observer's sampling tick. It is the finest interval a server can ask for, so every interval
/// the server may set is a whole number of ticks (PLAN §6.5).
pub(crate) const SAMPLE_TICK: Duration = Duration::from_secs(1);

/// The default interval of host and process metrics while the host is calm (PLAN §6.5).
pub(crate) const SAMPLE: Duration = Duration::from_secs(5);

/// The default interval of host and process metrics while the host is under pressure (PLAN §6.5).
pub(crate) const THROTTLED_INTERVAL: Duration = Duration::from_secs(15);

/// Bounds, in ms, of a sampling interval the server sets (the welcome's limits).
const INTERVAL_MIN_MS: u64 = 1_000;
const INTERVAL_MAX_MS: u64 = 300_000;

/// A server's sampling interval, in ms, clamped to [1 s, 300 s].
pub(crate) fn clamp_interval_ms(ms: u64) -> u64 {
    ms.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS)
}

/// The intervals of host and process metrics: the calm one, and the one under pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cadence {
    pub(crate) calm: Duration,
    pub(crate) pressure: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            calm: SAMPLE,
            pressure: THROTTLED_INTERVAL,
        }
    }
}

impl Cadence {
    /// The cadence the transport stored from the server's welcome, in ms, where 0 is absent and
    /// means the default.
    pub(crate) fn from_stored(calm_ms: u64, pressure_ms: u64) -> Self {
        let or = |ms: u64, default: Duration| {
            if ms == 0 {
                default
            } else {
                Duration::from_millis(ms)
            }
        };
        Self {
            calm: or(calm_ms, SAMPLE),
            pressure: or(pressure_ms, THROTTLED_INTERVAL),
        }
    }
}

const PSI_CPU_CRITICAL: f32 = 40.0;
const PSI_MEMORY_CRITICAL: f32 = 20.0;
const HOST_CPU_CRITICAL: f32 = 95.0;
/// Consecutive non-critical samples before a throttled agent samples normally again.
const CALM_SAMPLES: u32 = 3;

/// The auto-throttle of PLAN §6.5: while the host is under pressure, host and process metrics are
/// taken every [`THROTTLED_INTERVAL`] instead of every [`SAMPLE`], and the devices' guest CPU and
/// memory reads are skipped.
///
/// The host is critical when host PSI CPU `some avg10` is at least 40.0, or PSI memory `some
/// avg10` is at least 20.0, or host CPU usage is at least 95 %. macOS has no PSI, so there host CPU
/// alone decides. These thresholds are defaults, not configuration. The agent enters the throttled
/// state at once and leaves it only after three consecutive non-critical samples, so it does not
/// flap.
#[derive(Debug, Default)]
pub(crate) struct Throttle {
    critical: bool,
    /// Non-critical samples in a row while throttled.
    calm: u32,
    /// When the last sample was taken.
    last: Option<Instant>,
}

/// The host readings the throttle decides on: the ones a [`Sample`] carries. `None` is unknown and
/// never critical.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Pressure {
    host_cpu: Option<f32>,
    psi_cpu: Option<f32>,
    psi_memory: Option<f32>,
}

impl From<&Sample> for Pressure {
    fn from(sample: &Sample) -> Self {
        Self {
            host_cpu: sample.host_cpu_pct,
            psi_cpu: sample.host_psi_cpu_pct,
            psi_memory: sample.host_psi_memory_pct,
        }
    }
}

/// Whether any threshold of [`Throttle`] is reached.
fn is_critical(pressure: Pressure) -> bool {
    pressure.psi_cpu.is_some_and(|v| v >= PSI_CPU_CRITICAL)
        || pressure
            .psi_memory
            .is_some_and(|v| v >= PSI_MEMORY_CRITICAL)
        || pressure.host_cpu.is_some_and(|v| v >= HOST_CPU_CRITICAL)
}

impl Throttle {
    /// Whether the agent is throttled right now.
    pub(crate) fn throttled(&self) -> bool {
        self.critical
    }

    /// Whether a sample is due at `now`: none has been taken, or the interval of `cadence` for the
    /// current state has passed since the last one. Half a [`SAMPLE_TICK`] of slack absorbs the
    /// tick's scheduling jitter, so an interval is completed on the tick that ends it, not missed by
    /// a few milliseconds and not sampled a tick early.
    pub(crate) fn due(&self, now: Instant, cadence: Cadence) -> bool {
        let interval = if self.critical {
            cadence.pressure
        } else {
            cadence.calm
        };
        self.last
            .is_none_or(|t| now.saturating_duration_since(t) + SAMPLE_TICK / 2 >= interval)
    }

    /// Records a sample taken at `now` and its host readings.
    pub(crate) fn observe(&mut self, now: Instant, pressure: Pressure) {
        self.last = Some(now);
        if is_critical(pressure) {
            self.critical = true;
            self.calm = 0;
        } else if self.critical {
            self.calm = self.calm.saturating_add(1);
            if self.calm >= CALM_SAMPLES {
                self.critical = false;
                self.calm = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    #[test]
    fn samples_report_host_and_self() {
        let mut s = Sampler::new(Some(std::process::id()), None);
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let first = s.sample(at);
        assert!(first.host_mem_total_bytes.is_some_and(|t| t > 0));
        assert!(
            first.runner_rss_bytes.is_some(),
            "the watched pid (here: the test) is found"
        );
        assert!(first.agent_cpu_pct.is_none(), "no baseline yet");
        std::thread::sleep(Duration::from_millis(50));
        assert!(s.sample(at).agent_cpu_pct.is_some());
    }

    /// The child's accumulated CPU time, from the counter the sampler reads.
    fn cpu_time_of(pid: u32) -> Duration {
        let pid = Pid::from_u32(pid);
        let mut system = System::new_with_specifics(RefreshKind::nothing())
            .expect("system information should be available in tests");
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        system.process(pid).map_or(Duration::ZERO, |p| {
            Duration::from_millis(p.accumulated_cpu_time())
        })
    }

    /// A busy child standing in for the supervisor is counted in CPU and RSS. The CPU window is an
    /// injected second, and the test waits until the child has used 300 ms of CPU before it ends
    /// the window: the share the child gets on a loaded machine no longer decides the result.
    #[test]
    fn supervisor_is_included_when_given() {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let mut child = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .expect("sh should spawn");
        let start = Instant::now();
        let mut with = Sampler::new(None, Some(child.id()));
        let mut without = Sampler::new(None, None);
        let _ = (with.sample_at(at, start), without.sample_at(at, start));
        let before = cpu_time_of(child.id());
        let deadline = Instant::now() + Duration::from_secs(120);
        while cpu_time_of(child.id()) < before + Duration::from_millis(300) {
            assert!(
                Instant::now() < deadline,
                "the busy child never used 300 ms of CPU"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let end = start + Duration::from_secs(1);
        let (w, wo) = (with.sample_at(at, end), without.sample_at(at, end));
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            w.agent_cpu_pct.expect("cpu") > 20.0,
            "the busy process's CPU is in the sum: {w:?}"
        );
        assert!(
            w.agent_rss_bytes.expect("rss") > wo.agent_rss_bytes.expect("rss"),
            "its resident memory is in the sum"
        );
    }

    // UNVERIFIED: synthetic. The layout follows the kernel's pressure-stall-information
    // documentation (Documentation/accounting/psi.rst); no real /proc/pressure file was captured.
    #[test]
    fn psi_parser_reads_the_some_avg10_figure() {
        assert_eq!(
            parse_psi_some_avg10("some avg10=12.34 avg60=5.00 avg300=1.00 total=123456\n"),
            Some(12.34)
        );
        // A `full` line before it (or alone) must not be read as the `some` figure.
        assert_eq!(
            parse_psi_some_avg10(
                "full avg10=9.99 avg60=0.00 avg300=0.00 total=1\nsome avg10=0.50 avg60=0.00 avg300=0.00 total=2\n"
            ),
            Some(0.50)
        );
        assert_eq!(
            parse_psi_some_avg10("full avg10=9.99 avg60=0.00 avg300=0.00 total=1\n"),
            None
        );
        assert_eq!(parse_psi_some_avg10("some avg10=oops\n"), None);
        assert_eq!(parse_psi_some_avg10(""), None);
    }

    /// Load and swap come from sysinfo on every platform it supports; cores when it can count them.
    #[test]
    fn host_load_cores_and_swap_are_reported() {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let s = Sampler::new(None, None).sample(at);
        assert!(s.host_load_1m.is_some_and(|l| l >= 0.0));
        // Unknown is allowed (a container may hide the topology), zero is not.
        assert_ne!(s.host_cpu_cores, Some(0));
        assert!(s.host_swap_used_bytes.is_some() && s.host_swap_total_bytes.is_some());
    }

    /// Without Linux's PSI the pressure figures are unavailable, never zero.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pressure_is_unavailable_off_linux() {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let s = Sampler::new(None, None).sample(at);
        assert_eq!(
            (s.host_psi_cpu_pct, s.host_psi_memory_pct, s.host_psi_io_pct),
            (None, None, None)
        );
    }

    #[test]
    fn a_gone_supervisor_falls_back_to_the_observer() {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("true should spawn");
        let pid = child.id();
        child.wait().expect("wait");
        let mut s = Sampler::new(None, Some(pid));
        let _ = s.sample(at);
        std::thread::sleep(Duration::from_millis(20));
        let next = s.sample(at);
        assert!(next.agent_cpu_pct.is_some() && next.agent_rss_bytes.is_some());
    }

    /// Every reading of `s` that is unknown has a reason, and every known one has none (PLAN §6.5).
    /// Returns the names of the readings that break that rule.
    fn readings_without_matching_reason(s: &Sample) -> Vec<&'static str> {
        let u = s.unavailable;
        [
            (
                "host_cpu_pct",
                s.host_cpu_pct.is_some(),
                u.host_cpu_pct.is_some(),
            ),
            (
                "host_mem_used_bytes",
                s.host_mem_used_bytes.is_some(),
                u.host_mem_used_bytes.is_some(),
            ),
            (
                "host_mem_total_bytes",
                s.host_mem_total_bytes.is_some(),
                u.host_mem_total_bytes.is_some(),
            ),
            (
                "runner_cpu_pct",
                s.runner_cpu_pct.is_some(),
                u.runner_cpu_pct.is_some(),
            ),
            (
                "runner_rss_bytes",
                s.runner_rss_bytes.is_some(),
                u.runner_rss_bytes.is_some(),
            ),
            (
                "agent_cpu_pct",
                s.agent_cpu_pct.is_some(),
                u.agent_cpu_pct.is_some(),
            ),
            (
                "agent_rss_bytes",
                s.agent_rss_bytes.is_some(),
                u.agent_rss_bytes.is_some(),
            ),
            (
                "host_load_1m",
                s.host_load_1m.is_some(),
                u.host_load_1m.is_some(),
            ),
            (
                "host_cpu_cores",
                s.host_cpu_cores.is_some(),
                u.host_cpu_cores.is_some(),
            ),
            (
                "host_swap_used_bytes",
                s.host_swap_used_bytes.is_some(),
                u.host_swap_used_bytes.is_some(),
            ),
            (
                "host_swap_total_bytes",
                s.host_swap_total_bytes.is_some(),
                u.host_swap_total_bytes.is_some(),
            ),
            (
                "host_psi_cpu_pct",
                s.host_psi_cpu_pct.is_some(),
                u.host_psi_cpu_pct.is_some(),
            ),
            (
                "host_psi_memory_pct",
                s.host_psi_memory_pct.is_some(),
                u.host_psi_memory_pct.is_some(),
            ),
            (
                "host_psi_io_pct",
                s.host_psi_io_pct.is_some(),
                u.host_psi_io_pct.is_some(),
            ),
        ]
        .into_iter()
        .filter(|(_, has_value, has_reason)| has_value == has_reason)
        .map(|(name, _, _)| name)
        .collect()
    }

    /// A sampler that found no system information at start, as on a platform sysinfo doesn't cover.
    fn without_system(maestro: Option<Pid>) -> Sampler {
        Sampler {
            system: None,
            maestro,
            supervisor: None,
            last_self: None,
            last_supervisor: None,
            cpu_cores: Some(8),
        }
    }

    fn at() -> ClockSample {
        ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        }
    }

    /// The first sample of a run says why each reading is unknown, and a read that has a value has
    /// no reason: on the real system and without it, with and without a Maestro process.
    #[test]
    fn every_unknown_reading_of_a_sample_has_its_reason() {
        let mut real = Sampler::new(Some(std::process::id()), None);
        let first = real.sample(at());
        assert_eq!(
            first.unavailable.agent_cpu_pct,
            Some(Unavailable::NoBaseline),
            "the first sample only sets the agent's CPU baseline"
        );
        assert_eq!(
            readings_without_matching_reason(&first),
            Vec::<&str>::new(),
            "{first:?}"
        );
        assert_eq!(
            readings_without_matching_reason(&real.sample(at())),
            Vec::<&str>::new()
        );

        let mut unknown_maestro = Sampler::new(Some(u32::MAX - 1), None);
        let s = unknown_maestro.sample(at());
        assert_eq!(
            (s.runner_cpu_pct, s.runner_rss_bytes),
            (None, None),
            "a pid the table does not hold is not a reading"
        );
        assert_eq!(
            (s.unavailable.runner_cpu_pct, s.unavailable.runner_rss_bytes),
            (Some(Unavailable::NoProcess), Some(Unavailable::NoProcess))
        );
        assert_eq!(readings_without_matching_reason(&s), Vec::<&str>::new());

        let s = without_system(None).sample(at());
        assert_eq!(
            readings_without_matching_reason(&s),
            Vec::<&str>::new(),
            "{s:?}"
        );
    }

    /// Without the platform's system information nothing from it is read, and each of those
    /// readings says so. The core count is read at start, so it is still known.
    #[test]
    fn a_platform_without_system_information_says_so_for_each_reading() {
        let s = without_system(None).sample(at());
        assert_eq!(
            (
                s.host_cpu_pct,
                s.host_mem_used_bytes,
                s.host_swap_used_bytes
            ),
            (None, None, None)
        );
        assert_eq!(
            (
                s.unavailable.host_cpu_pct,
                s.unavailable.host_mem_total_bytes,
                s.unavailable.host_psi_io_pct,
                s.unavailable.agent_rss_bytes,
            ),
            (
                Some(Unavailable::NoSystemInfo),
                Some(Unavailable::NoSystemInfo),
                Some(Unavailable::NoSystemInfo),
                Some(Unavailable::NoSystemInfo),
            )
        );
        assert_eq!(s.host_cpu_cores, Some(8));
        assert_eq!(s.unavailable.host_cpu_cores, None);
    }

    /// A kernel without PSI (no `/proc/pressure` file, as on macOS) says so; a file that cannot be
    /// parsed is a failed probe. A file with a figure is the figure.
    #[test]
    fn psi_says_no_psi_for_a_missing_file_and_a_failed_probe_for_a_bad_one() {
        let dir = std::env::temp_dir().join(format!("fleet-psi-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let good = dir.join("cpu");
        std::fs::write(&good, "some avg10=1.25 avg60=0.00 avg300=0.00 total=9\n").expect("write");
        let bad = dir.join("memory");
        std::fs::write(&bad, "garbage\n").expect("write");
        let missing = dir.join("io");

        assert_eq!(read_psi_file(&good), Ok(1.25));
        assert_eq!(read_psi_file(&bad), Err(Unavailable::ProbeFailed));
        assert_eq!(read_psi_file(&missing), Err(Unavailable::NoPsi));
        let _ = std::fs::remove_dir_all(&dir);
    }

    const CALM: Pressure = Pressure {
        host_cpu: Some(10.0),
        psi_cpu: Some(0.0),
        psi_memory: Some(0.0),
    };

    #[test]
    fn each_threshold_is_critical_at_its_value_and_not_below() {
        let with = |p: Pressure| is_critical(p);
        assert!(!with(CALM));
        assert!(with(Pressure {
            psi_cpu: Some(40.0),
            ..CALM
        }));
        assert!(!with(Pressure {
            psi_cpu: Some(39.9),
            ..CALM
        }));
        assert!(with(Pressure {
            psi_memory: Some(20.0),
            ..CALM
        }));
        assert!(!with(Pressure {
            psi_memory: Some(19.9),
            ..CALM
        }));
        assert!(with(Pressure {
            host_cpu: Some(95.0),
            ..CALM
        }));
        assert!(!with(Pressure {
            host_cpu: Some(94.9),
            ..CALM
        }));
    }

    /// macOS has no PSI: the figures are unknown there, and host CPU alone decides. Unknown never
    /// counts as critical.
    #[test]
    fn without_psi_host_cpu_alone_decides() {
        let no_psi = |cpu| Pressure {
            host_cpu: cpu,
            psi_cpu: None,
            psi_memory: None,
        };
        assert!(is_critical(no_psi(Some(96.0))));
        assert!(!is_critical(no_psi(Some(60.0))));
        assert!(!is_critical(no_psi(None)));
        assert!(!is_critical(Pressure::default()));
    }

    #[test]
    fn throttling_enters_at_once_and_leaves_after_three_calm_samples() {
        let t0 = Instant::now();
        let mut throttle = Throttle::default();
        let hot = Pressure {
            psi_memory: Some(25.0),
            ..CALM
        };
        throttle.observe(t0, hot);
        assert!(throttle.throttled(), "one critical sample is enough");
        let mut at = t0;
        for _ in 0..2 {
            at += SAMPLE;
            throttle.observe(at, CALM);
        }
        assert!(throttle.throttled(), "two calm samples do not leave");
        at += SAMPLE;
        throttle.observe(at, hot);
        for _ in 0..2 {
            at += SAMPLE;
            throttle.observe(at, CALM);
        }
        assert!(
            throttle.throttled(),
            "a critical sample restarts the count of calm ones"
        );
        at += SAMPLE;
        throttle.observe(at, CALM);
        assert!(!throttle.throttled(), "three calm samples in a row leave");
    }

    /// An unreadable reading is unknown, not critical: it never throttles the agent, and while
    /// throttled it counts as a calm sample, so the agent keeps sampling and can leave the state.
    #[test]
    fn unreadable_readings_never_throttle_and_count_as_calm() {
        let t0 = Instant::now();
        let mut throttle = Throttle::default();
        throttle.observe(t0, Pressure::default());
        assert!(!throttle.throttled());
        assert!(
            throttle.due(t0 + SAMPLE, Cadence::default()),
            "an unreadable host keeps sampling"
        );
        let hot = Pressure {
            host_cpu: Some(99.0),
            ..Pressure::default()
        };
        throttle.observe(t0 + SAMPLE, hot);
        assert!(throttle.throttled());
        let mut at = t0 + SAMPLE;
        for _ in 0..CALM_SAMPLES {
            at += THROTTLED_INTERVAL;
            throttle.observe(at, Pressure::default());
        }
        assert!(!throttle.throttled());
    }

    #[test]
    fn samples_are_due_every_five_seconds_when_calm_and_every_fifteen_when_throttled() {
        let cadence = Cadence::default();
        let t0 = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.due(t0, cadence), "the first sample is due at once");
        throttle.observe(t0, CALM);
        assert!(!throttle.due(t0 + Duration::from_secs(2), cadence));
        assert!(!throttle.due(t0 + Duration::from_secs(4), cadence));
        assert!(throttle.due(t0 + SAMPLE, cadence));
        assert!(
            throttle.due(t0 + Duration::from_millis(4990), cadence),
            "tick jitter does not skip a sample"
        );
        let hot_at = t0 + SAMPLE;
        throttle.observe(
            hot_at,
            Pressure {
                host_cpu: Some(97.0),
                ..CALM
            },
        );
        assert!(
            !throttle.due(hot_at + SAMPLE, cadence),
            "throttled: not after 5 s"
        );
        assert!(!throttle.due(hot_at + Duration::from_secs(12), cadence));
        assert!(throttle.due(hot_at + Duration::from_millis(14_990), cadence));
    }

    #[test]
    fn a_two_second_calm_interval_and_a_thirty_second_pressure_interval_are_honoured() {
        let cadence = Cadence {
            calm: Duration::from_secs(2),
            pressure: Duration::from_secs(30),
        };
        let t0 = Instant::now();
        let mut calm = Throttle::default();
        calm.observe(t0, CALM);
        assert!(!calm.due(t0 + Duration::from_secs(1), cadence));
        assert!(calm.due(t0 + Duration::from_secs(2), cadence));

        let mut hot = Throttle::default();
        hot.observe(
            t0,
            Pressure {
                host_cpu: Some(97.0),
                ..CALM
            },
        );
        assert!(
            !hot.due(t0 + Duration::from_secs(15), cadence),
            "under pressure: not at 15 s"
        );
        assert!(hot.due(t0 + Duration::from_secs(30), cadence));
    }

    #[test]
    fn the_clamp_keeps_a_server_interval_within_one_second_and_five_minutes() {
        assert_eq!(clamp_interval_ms(0), 1_000);
        assert_eq!(clamp_interval_ms(999), 1_000);
        assert_eq!(clamp_interval_ms(1_000), 1_000);
        assert_eq!(clamp_interval_ms(2_000), 2_000);
        assert_eq!(clamp_interval_ms(300_000), 300_000);
        assert_eq!(clamp_interval_ms(300_001), 300_000);
        assert_eq!(clamp_interval_ms(999_999_999), 300_000);
    }

    #[test]
    fn stored_zero_is_absent_and_gives_the_defaults() {
        assert_eq!(Cadence::from_stored(0, 0), Cadence::default());
        assert_eq!(
            Cadence::from_stored(2_000, 0),
            Cadence {
                calm: Duration::from_secs(2),
                pressure: THROTTLED_INTERVAL,
            }
        );
    }
}
