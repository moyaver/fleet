// SPDX-License-Identifier: AGPL-3.0-only
//! Host, Maestro and agent metrics (PLAN §6.5, research 02 §2; invariant I10).
//!
//! Only targeted refreshes: host CPU and memory, and one process (Maestro's JVM). The agent's own
//! cost, supervisor plus observer, is a reported number, not a guess: the observer's CPU comes from
//! `getrusage`, the supervisor's from a targeted refresh of its one PID, and resident memory of
//! both from the same refresh.

use std::time::{Duration, Instant};

use fleet_protocol::{frame::Sample, journal::ClockSample};
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

    /// One sample.
    pub(crate) fn sample(&mut self, at: ClockSample) -> Sample {
        let (agent_cpu_pct, agent_rss_bytes) = self.agent_usage();
        let empty = Sample {
            at,
            host_cpu_pct: None,
            host_mem_used_bytes: None,
            host_mem_total_bytes: None,
            maestro_cpu_pct: None,
            maestro_rss_bytes: None,
            agent_cpu_pct,
            agent_rss_bytes,
            host_load_1m: None,
            host_cpu_cores: None,
            host_swap_used_bytes: None,
            host_swap_total_bytes: None,
            host_psi_cpu_pct: None,
            host_psi_memory_pct: None,
            host_psi_io_pct: None,
        };
        let Some(system) = self.system.as_mut() else {
            return empty;
        };
        system.refresh_cpu_usage();
        system.refresh_memory();
        let (maestro_cpu_pct, maestro_rss_bytes) = match self.maestro {
            Some(pid) => {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_cpu().with_memory(),
                );
                system
                    .process(pid)
                    .map_or((None, None), |p| (Some(p.cpu_usage()), Some(p.memory())))
            }
            None => (None, None),
        };
        let total = system.total_memory();
        Sample {
            host_cpu_pct: Some(system.global_cpu_usage()),
            host_mem_used_bytes: (total > 0).then(|| system.used_memory()),
            host_mem_total_bytes: (total > 0).then_some(total),
            maestro_cpu_pct,
            maestro_rss_bytes,
            host_load_1m: System::load_average().ok().as_ref().map(load_one),
            host_cpu_cores: self.cpu_cores,
            host_swap_used_bytes: Some(system.used_swap()),
            host_swap_total_bytes: Some(system.total_swap()),
            host_psi_cpu_pct: read_psi("cpu"),
            host_psi_memory_pct: read_psi("memory"),
            host_psi_io_pct: read_psi("io"),
            ..empty
        }
    }

    /// The agent's cost (observer plus supervisor): CPU since the previous sample as a percentage
    /// of one core (sum of both processes' CPU-time deltas, each against its own baseline), and
    /// the current resident memory of both. When the supervisor is gone or unknown, the numbers
    /// cover the observer only.
    fn agent_usage(&mut self) -> (Option<f32>, Option<u64>) {
        let now = Instant::now();
        let own = getrusage(UsageWho::RUSAGE_SELF)
            .ok()
            .map(|u| tv(u.user_time()) + tv(u.system_time()));
        let mut rss = None;
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
            rss = own_rss.map(|r| r + sup.map_or(0, sysinfo::Process::memory));
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
                Some(p)
            }
            _ => None,
        };
        if let Some(cpu) = own {
            self.last_self = Some((now, cpu));
        }
        (pct, rss)
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
/// the last ten seconds. `None` where there is no PSI (macOS, or a kernel built without it).
fn read_psi(resource: &str) -> Option<f32> {
    parse_psi_some_avg10(&std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?)
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
            first.maestro_rss_bytes.is_some(),
            "the watched pid (here: the test) is found"
        );
        assert!(first.agent_cpu_pct.is_none(), "no baseline yet");
        std::thread::sleep(Duration::from_millis(50));
        assert!(s.sample(at).agent_cpu_pct.is_some());
    }

    /// A busy child standing in for the supervisor is counted in CPU and RSS.
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
        let mut with = Sampler::new(None, Some(child.id()));
        let mut without = Sampler::new(None, None);
        let _ = (with.sample(at), without.sample(at));
        std::thread::sleep(Duration::from_millis(300));
        let (w, wo) = (with.sample(at), without.sample(at));
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
}
