// SPDX-License-Identifier: AGPL-3.0-only
//! Host, Maestro and agent metrics (PLAN §6.5, research 02 §2; invariant I10).
//!
//! Only targeted refreshes: host CPU and memory, and one process (Maestro's JVM). The agent's own
//! cost comes from `getrusage`, so its overhead is a reported number, not a guess.

use std::time::{Duration, Instant};

use fleet_protocol::{frame::Sample, journal::ClockSample};
use nix::sys::resource::{UsageWho, getrusage};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Samples metrics; keep one per run so CPU percentages have a baseline.
pub(crate) struct Sampler {
    /// `None` when the platform's system information is unavailable: samples say so.
    system: Option<System>,
    maestro: Option<Pid>,
    last_self: Option<(Instant, Duration)>,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("maestro", &self.maestro)
            .finish_non_exhaustive()
    }
}

impl Sampler {
    pub(crate) fn new(maestro_pid: Option<u32>) -> Self {
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
            last_self: None,
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
            ..empty
        }
    }

    /// The observer's CPU since the previous sample, as a percentage of one core, and its peak RSS.
    fn agent_usage(&mut self) -> (Option<f32>, Option<u64>) {
        let Ok(usage) = getrusage(UsageWho::RUSAGE_SELF) else {
            return (None, None);
        };
        let cpu = tv(usage.user_time()) + tv(usage.system_time());
        // ru_maxrss is bytes on macOS and KiB on Linux.
        let rss = u64::try_from(usage.max_rss()).ok().map(|r| {
            if cfg!(target_os = "macos") {
                r
            } else {
                r * 1024
            }
        });
        let now = Instant::now();
        let pct = self.last_self.map(|(then, before)| {
            let wall = now.duration_since(then).as_secs_f64();
            #[expect(clippy::cast_possible_truncation, reason = "a percentage fits in f32")]
            let p = if wall > 0.0 {
                ((cpu.saturating_sub(before)).as_secs_f64() / wall * 100.0) as f32
            } else {
                0.0
            };
            p
        });
        self.last_self = Some((now, cpu));
        (pct, rss)
    }
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
        let mut s = Sampler::new(Some(std::process::id()));
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
}
