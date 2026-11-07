// SPDX-License-Identifier: AGPL-3.0-only
//! Device samples (PLAN §6.5 Devices; research 02 §4.2; invariants I2, I3, I4, I10).
//!
//! For each device Maestro drives, one read-only probe runs every [`INTERVAL`]: a single shell
//! command through the adb server that prints the boot flag, the guest's aggregate CPU counters
//! and `MemAvailable`. adb's state for the device comes from the device list adb already pushes,
//! so it costs no call. A probe runs in its own task with a [`TIMEOUT`], and at most one is in
//! flight per device, so a slow or dead device never holds up the observer (I10).
//!
//! Fail-open (I2): a probe that fails leaves its readings unknown, the first failure of a run is
//! logged once, and nothing reaches Maestro. Unknown is never reported as zero (I3, I4): a reading
//! is set only when adb answered with it.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fleet_protocol::{event::AdbState, frame::DeviceSample, journal::ClockSample};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::adb;

/// The probe: one shell command. It has no loop, so nothing is left running on the device.
/// Captured output: `fixtures/adb/android-37.1/probe.raw`.
pub(crate) const PROBE: &str =
    r#"echo "boot=$(getprop sys.boot_completed)"; head -n 1 /proc/stat; cat /proc/meminfo"#;

/// Reads the device's API level, once per device session (PLAN §6.5). Captured output:
/// `fixtures/adb/android-37.1/api-level.raw`.
pub(crate) const API_LEVEL: &str = "getprop ro.build.version.sdk";

/// Lists the activities, to find the resumed one after a failure. Captured output:
/// `fixtures/adb/android-37.1/dumpsys-activity-activities.raw`.
pub(crate) const ACTIVITY: &str = "dumpsys activity activities";

/// Time between two probes of one device. The guest figures are sampled at 30 s or more (PLAN §6.5).
pub(crate) const INTERVAL: Duration = Duration::from_secs(30);

/// The longest wait for one probe. A probe that takes longer is abandoned and its readings are
/// unknown.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// What one probe read from the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Probe {
    /// Whether `sys.boot_completed` was `1`. An unset property (boot not finished) is `false`.
    pub(crate) boot_completed: bool,
    /// The guest's aggregate CPU counters, when the `cpu` line was there.
    pub(crate) cpu: Option<CpuTicks>,
    /// `MemAvailable`, bytes, when that line was there.
    pub(crate) mem_available_bytes: Option<u64>,
}

/// The aggregate CPU counters of `/proc/stat`, in clock ticks: busy time and total time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuTicks {
    busy: u64,
    total: u64,
}

/// Reads a probe's output. `None` when the boot line is missing: the reply is not a probe's.
pub(crate) fn parse(text: &str) -> Option<Probe> {
    let mut boot_completed = None;
    let mut cpu = None;
    let mut mem_available_bytes = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("boot=") {
            boot_completed = Some(value.trim() == "1");
        } else if let Some(counters) = line.strip_prefix("cpu ") {
            cpu = cpu_ticks(counters);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            mem_available_bytes = rest
                .split_whitespace()
                .next()
                .and_then(|kib| kib.parse::<u64>().ok())
                .and_then(|kib| kib.checked_mul(1024));
        }
    }
    Some(Probe {
        boot_completed: boot_completed?,
        cpu,
        mem_available_bytes,
    })
}

/// Reads [`API_LEVEL`]'s output: the level as a number, `None` when it is not one.
pub(crate) fn parse_api_level(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

/// The component (`package/.Activity`) of the resumed activity in [`ACTIVITY`]'s output. adb
/// prints it as `ResumedActivity: ActivityRecord{<hash> u<user> <component> t<task>}`, and the
/// component is the token with a slash. `None` when no such line is there.
pub(crate) fn resumed_activity(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("ResumedActivity:") || l.starts_with("mResumedActivity:"))?;
    let record = line.split_once("ActivityRecord{")?.1.split('}').next()?;
    record
        .split_whitespace()
        .find(|token| token.contains('/'))
        .map(str::to_owned)
}

/// The `cpu` line's counters: user nice system idle iowait irq softirq steal (guest time is
/// already in user and nice). Idle counts iowait, as `top` does.
fn cpu_ticks(counters: &str) -> Option<CpuTicks> {
    let fields = counters
        .split_whitespace()
        .map(|f| f.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    let at = |i: usize| fields.get(i).copied().unwrap_or(0);
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().take(8).sum();
    let idle = at(3).saturating_add(at(4));
    Some(CpuTicks {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Percent of the guest's CPU used between two probes of one device. The first probe only sets
/// the baseline, and a counter that went backwards (the guest restarted) starts over.
#[derive(Debug, Default)]
struct GuestCpu {
    last: Option<CpuTicks>,
}

impl GuestCpu {
    fn percent(&mut self, now: CpuTicks) -> Option<f32> {
        let before = self.last.replace(now)?;
        let total = now.total.checked_sub(before.total)?;
        let busy = now.busy.checked_sub(before.busy)?;
        if total == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "tick counts stay far below 2^53; a percentage fits in f32"
        )]
        let pct = (busy as f64 / total as f64 * 100.0) as f32;
        Some(pct.clamp(0.0, 100.0))
    }
}

/// A device to probe, as the session knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The serial Maestro selected for a shard.
    pub(crate) serial: String,
    /// The device session of that shard: the samples belong to it.
    pub(crate) device_session_id: Uuid,
    /// adb's state for the serial, from the latest device list (`None`: no list yet).
    pub(crate) adb_state: Option<AdbState>,
}

/// One device's probing state.
#[derive(Debug, Default)]
struct Watch {
    in_flight: bool,
    last_started: Option<Instant>,
    cpu: GuestCpu,
    /// The API level, once a probe has read it.
    api_level: Option<u32>,
}

/// What a finished task read.
#[derive(Debug)]
enum Read {
    /// A probe, and the API level if this one also read it.
    Probe {
        probe: io::Result<Probe>,
        api_level: Option<u32>,
    },
    /// The resumed activity's component, if the listing named one.
    Activity(io::Result<Option<String>>),
}

/// A finished task, from its task to the observer's next tick.
#[derive(Debug)]
struct Done {
    target: Target,
    at: ClockSample,
    read: Read,
}

/// The device probes of one run. Probes run in tasks; the observer collects their samples with
/// [`take`](Self::take) on its sampling tick, so no probe ever waits in the observer's loop.
#[derive(Debug)]
pub(crate) struct Probes {
    addr: String,
    timeout: Duration,
    watch: HashMap<String, Watch>,
    /// Probes that finished since the last [`take`](Self::take). At most one per device in flight,
    /// so this stays as small as the device list.
    finished: Arc<Mutex<Vec<Done>>>,
    /// Woken when a probe finishes (tests wait on it).
    woken: Arc<Notify>,
    /// Whether the first failure was logged; later ones are debug lines.
    warned: bool,
}

impl Probes {
    /// Probes through the adb server at `addr`, each abandoned after [`TIMEOUT`].
    pub(crate) fn new(addr: String) -> Self {
        Self::with_timeout(addr, TIMEOUT)
    }

    /// As [`new`](Self::new), with another probe timeout (tests wait less).
    pub(crate) fn with_timeout(addr: String, timeout: Duration) -> Self {
        Self {
            addr,
            timeout,
            watch: HashMap::new(),
            finished: Arc::new(Mutex::new(Vec::new())),
            woken: Arc::new(Notify::new()),
            warned: false,
        }
    }

    /// Starts a probe for each target that is due: none in flight for its serial, and the last
    /// one started at least [`INTERVAL`] ago. Never waits for a probe.
    pub(crate) fn start_due(&mut self, now: Instant, at: &ClockSample, targets: &[Target]) {
        for target in targets {
            let watch = self.watch.entry(target.serial.clone()).or_default();
            let due = !watch.in_flight
                && watch
                    .last_started
                    .is_none_or(|t| now.duration_since(t) >= INTERVAL);
            if !due {
                continue;
            }
            watch.in_flight = true;
            watch.last_started = Some(now);
            let want_api_level = watch.api_level.is_none();
            let (addr, timeout, finished, woken, target, at) = (
                self.addr.clone(),
                self.timeout,
                Arc::clone(&self.finished),
                Arc::clone(&self.woken),
                target.clone(),
                *at,
            );
            tokio::spawn(async move {
                let result = tokio::time::timeout(
                    timeout,
                    read_probe(&addr, &target.serial, want_api_level),
                )
                .await;
                let read = match result {
                    Ok((probe, api_level)) => Read::Probe { probe, api_level },
                    Err(_) => Read::Probe {
                        probe: Err(io::Error::new(io::ErrorKind::TimedOut, "probe timed out")),
                        api_level: None,
                    },
                };
                push(&finished, &woken, Done { target, at, read });
            });
        }
    }

    /// Starts a read of the resumed activity for each target, now. Used after a flow failed on a
    /// device (PLAN §6.5: current activity only on failure). Not deduplicated here: the observer
    /// asks once per device per tick.
    pub(crate) fn request_activity(&mut self, at: &ClockSample, targets: &[Target]) {
        for target in targets {
            let (addr, timeout, finished, woken, target, at) = (
                self.addr.clone(),
                self.timeout,
                Arc::clone(&self.finished),
                Arc::clone(&self.woken),
                target.clone(),
                *at,
            );
            tokio::spawn(async move {
                let result = match tokio::time::timeout(
                    timeout,
                    adb::shell(&addr, &target.serial, ACTIVITY),
                )
                .await
                {
                    Ok(Ok(bytes)) => Ok(resumed_activity(&String::from_utf8_lossy(&bytes))),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "activity read timed out",
                    )),
                };
                push(
                    &finished,
                    &woken,
                    Done {
                        target,
                        at,
                        read: Read::Activity(result),
                    },
                );
            });
        }
    }

    /// The samples of the tasks that finished since the last call, in finishing order.
    pub(crate) fn take(&mut self) -> Vec<DeviceSample> {
        let done = match self.finished.lock() {
            Ok(mut finished) => std::mem::take(&mut *finished),
            Err(_) => return Vec::new(),
        };
        done.into_iter().filter_map(|d| self.finish(d)).collect()
    }

    /// Returns once a task has finished (or at once if one finished since the last wait).
    #[cfg(test)]
    async fn finished_one(&self) {
        self.woken.notified().await;
    }

    /// The sample a finished task makes, or `None` when it has nothing to report (a failed or
    /// empty activity read: unknown, so nothing is sent).
    fn finish(&mut self, done: Done) -> Option<DeviceSample> {
        let serial = done.target.serial.clone();
        let watch = self.watch.entry(serial.clone()).or_default();
        match done.read {
            Read::Probe { probe, api_level } => {
                watch.in_flight = false;
                if let Some(level) = api_level {
                    watch.api_level = Some(level);
                }
                let (boot_completed, guest_cpu_pct, guest_mem_available_bytes) = match probe {
                    Ok(probe) => (
                        Some(probe.boot_completed),
                        probe.cpu.and_then(|now| watch.cpu.percent(now)),
                        probe.mem_available_bytes,
                    ),
                    Err(e) => {
                        log_failure(&mut self.warned, &serial, &e);
                        (None, None, None)
                    }
                };
                Some(DeviceSample {
                    at: done.at,
                    device_session_id: done.target.device_session_id,
                    adb_state: done.target.adb_state,
                    boot_completed,
                    guest_cpu_pct,
                    guest_mem_available_bytes,
                    emu_cpu_pct: None,
                    emu_rss_bytes: None,
                    avd_name: None,
                    api_level: watch.api_level,
                    foreground_activity: None,
                })
            }
            Read::Activity(result) => match result {
                Ok(Some(component)) => Some(DeviceSample {
                    at: done.at,
                    device_session_id: done.target.device_session_id,
                    adb_state: done.target.adb_state,
                    boot_completed: None,
                    guest_cpu_pct: None,
                    guest_mem_available_bytes: None,
                    emu_cpu_pct: None,
                    emu_rss_bytes: None,
                    avd_name: None,
                    api_level: watch.api_level,
                    foreground_activity: Some(component),
                }),
                Ok(None) => None,
                Err(e) => {
                    log_failure(&mut self.warned, &serial, &e);
                    None
                }
            },
        }
    }
}

/// Logs the first failure of the run at warn level, and later ones at debug level. The reading
/// stays unknown either way.
fn log_failure(warned: &mut bool, serial: &str, error: &io::Error) {
    if *warned {
        tracing::debug!(%serial, error = %error, "device read failed");
    } else {
        *warned = true;
        tracing::warn!(
            %serial,
            error = %error,
            "device read failed; device readings are unknown until one succeeds"
        );
    }
}

/// One probe task's reads: the API level first when it is still unknown, then the probe. A
/// failed API read is not an error of the probe (fail-open): the level simply stays unknown.
async fn read_probe(
    addr: &str,
    serial: &str,
    want_api_level: bool,
) -> (io::Result<Probe>, Option<u32>) {
    let api_level = if want_api_level {
        adb::shell(addr, serial, API_LEVEL)
            .await
            .ok()
            .and_then(|bytes| parse_api_level(&String::from_utf8_lossy(&bytes)))
    } else {
        None
    };
    let probe = match adb::shell(addr, serial, PROBE).await {
        Ok(bytes) => parse(&String::from_utf8_lossy(&bytes))
            .ok_or_else(|| io::Error::other("the probe's reply had no boot line")),
        Err(e) => Err(e),
    };
    (probe, api_level)
}

/// Hands a finished task to the observer and wakes anyone waiting for one.
fn push(finished: &Mutex<Vec<Done>>, woken: &Notify, done: Done) {
    if let Ok(mut finished) = finished.lock() {
        finished.push(done);
    }
    woken.notify_one();
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::adb::fake;

    /// Waits until `n` probes have finished and returns their samples.
    async fn collect(probes: &mut Probes, n: usize) -> Vec<DeviceSample> {
        let mut out = Vec::new();
        while out.len() < n {
            probes.finished_one().await;
            out.extend(probes.take());
        }
        out
    }

    /// Captured from an emulator (fixtures/adb/android-37.1, see its README). Each file is the
    /// transport's `OKAY`, the shell's `OKAY`, then what the probe printed.
    const FIRST: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/probe.raw");
    const LATER: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/probe-2.raw");
    const UNSET: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/probe-unset-prop.raw");
    const API: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/api-level.raw");
    const DUMPSYS: &[u8] =
        include_bytes!("../../../fixtures/adb/android-37.1/dumpsys-activity-activities.raw");

    /// The replies of a device that answers both the probe and the API level read.
    fn answers_probe_and_level() -> Vec<(&'static str, &'static [u8])> {
        vec![(PROBE, &FIRST[4..]), (API_LEVEL, &API[4..])]
    }

    fn text(captured: &[u8]) -> String {
        String::from_utf8_lossy(&captured[8..]).into_owned()
    }

    fn at(mono_ms: u64) -> ClockSample {
        ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms,
        }
    }

    fn target(serial: &str, adb_state: Option<AdbState>) -> Target {
        Target {
            serial: serial.into(),
            device_session_id: Uuid::from_u128(7),
            adb_state,
        }
    }

    #[test]
    fn the_captured_probe_reads_boot_cpu_and_memory() {
        let probe = parse(&text(FIRST)).expect("a probe's reply");
        assert!(probe.boot_completed);
        assert_eq!(probe.mem_available_bytes, Some(2_268_736 * 1024));
        assert!(probe.cpu.is_some());
    }

    #[test]
    fn the_api_level_and_the_resumed_activity_parse_from_their_captures() {
        assert_eq!(parse_api_level(&text(API)), Some(37));
        assert_eq!(parse_api_level("oops\n"), None);
        assert_eq!(
            resumed_activity(&text(DUMPSYS)).as_deref(),
            Some("com.android.fakesystemapp/.launcher.EmptyHomeActivity")
        );
        assert_eq!(resumed_activity(""), None, "no listing, no activity");
    }

    #[test]
    fn an_unset_boot_property_means_not_completed_and_a_reply_without_one_is_unknown() {
        assert!(!parse(&text(UNSET)).expect("a probe's reply").boot_completed);
        assert_eq!(parse("cpu  1 2 3 4\nMemAvailable:  5 kB\n"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn guest_cpu_is_the_busy_share_between_two_probes() {
        let first = parse(&text(FIRST)).unwrap().cpu.unwrap();
        let second = parse(&text(LATER)).unwrap().cpu.unwrap();
        let mut guest = GuestCpu::default();
        assert_eq!(
            guest.percent(first),
            None,
            "the first probe is the baseline"
        );
        // 4250 busy ticks out of 6763 between the two captures.
        let pct = guest.percent(second).unwrap();
        assert!((pct - 62.84).abs() < 0.05, "{pct}");
    }

    #[test]
    fn a_counter_that_went_backwards_starts_over() {
        let mut guest = GuestCpu::default();
        let high = CpuTicks {
            busy: 500,
            total: 1000,
        };
        let low = CpuTicks {
            busy: 10,
            total: 100,
        };
        assert_eq!(guest.percent(high), None);
        assert_eq!(guest.percent(low), None, "the guest restarted: no figure");
        assert_eq!(
            guest.percent(CpuTicks {
                busy: 20,
                total: 200
            }),
            Some(10.0)
        );
    }

    #[tokio::test]
    async fn a_device_that_answers_gives_its_readings_and_adbs_state() {
        let (addr, _) = fake::devices("emulator-5584", answers_probe_and_level()).await;
        let mut probes = Probes::new(addr);
        probes.start_due(
            Instant::now(),
            &at(0),
            &[target("emulator-5584", Some(AdbState::Device))],
        );
        let s = collect(&mut probes, 1).await.remove(0);
        assert_eq!(
            (s.boot_completed, s.adb_state, s.guest_cpu_pct),
            (Some(true), Some(AdbState::Device), None),
            "the first probe has no CPU baseline"
        );
        assert_eq!(s.guest_mem_available_bytes, Some(2_268_736 * 1024));
        assert_eq!(s.device_session_id, Uuid::from_u128(7));
        assert_eq!(s.api_level, Some(37));
        assert_eq!(
            (s.avd_name, s.emu_cpu_pct, s.foreground_activity),
            (None, None, None)
        );
    }

    #[tokio::test]
    async fn the_api_level_is_read_once_and_then_carried_on_every_sample() {
        let (addr, connections) = fake::devices("emulator-5584", answers_probe_and_level()).await;
        let mut probes = Probes::new(addr);
        let targets = [target("emulator-5584", Some(AdbState::Device))];
        let start = Instant::now();
        probes.start_due(start, &at(0), &targets);
        let first = collect(&mut probes, 1).await.remove(0);
        probes.start_due(start + INTERVAL, &at(30_000), &targets);
        let second = collect(&mut probes, 1).await.remove(0);
        assert_eq!((first.api_level, second.api_level), (Some(37), Some(37)));
        // One API read and two probes: the level is not asked for again once it is known.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_resumed_activity_is_reported_on_its_own_sample() {
        let (addr, _) = fake::device("emulator-5584", ACTIVITY, &DUMPSYS[4..]).await;
        let mut probes = Probes::new(addr);
        probes.request_activity(&at(0), &[target("emulator-5584", Some(AdbState::Device))]);
        let s = collect(&mut probes, 1).await.remove(0);
        assert_eq!(
            (
                s.foreground_activity.as_deref(),
                s.boot_completed,
                s.guest_cpu_pct
            ),
            (
                Some("com.android.fakesystemapp/.launcher.EmptyHomeActivity"),
                None,
                None
            ),
        );
        assert_eq!(s.adb_state, Some(AdbState::Device));
    }

    #[tokio::test]
    async fn a_failed_activity_read_reports_nothing() {
        // The fake answers no `dumpsys` command, so adb's refusal comes back.
        let (addr, _) = fake::devices("emulator-5584", Vec::new()).await;
        let mut probes = Probes::new(addr);
        probes.request_activity(&at(0), &[target("emulator-5584", None)]);
        probes.finished_one().await;
        assert!(probes.take().is_empty(), "unknown is not reported");
    }

    #[tokio::test]
    async fn one_probe_per_device_per_interval_and_never_two_at_once() {
        let (addr, connections) = fake::devices("emulator-5584", answers_probe_and_level()).await;
        let mut probes = Probes::new(addr);
        let targets = [target("emulator-5584", Some(AdbState::Device))];
        let start = Instant::now();
        probes.start_due(start, &at(0), &targets);
        // Ten seconds later the device is not due: the probe starting then is never spawned.
        probes.start_due(start + Duration::from_secs(10), &at(10_000), &targets);
        assert_eq!(collect(&mut probes, 1).await[0].at.mono_ms, 0);
        // Thirty seconds after the first probe, the next one is due.
        probes.start_due(start + INTERVAL, &at(30_000), &targets);
        assert_eq!(collect(&mut probes, 1).await[0].at.mono_ms, 30_000);
        // The API read, then one probe, then the next probe: three connections.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn no_second_probe_starts_while_one_is_in_flight() {
        let (addr, connections) = fake::devices("emulator-5584", answers_probe_and_level()).await;
        let mut probes = Probes::new(addr);
        let targets = [target("emulator-5584", None)];
        let start = Instant::now();
        probes.start_due(start, &at(0), &targets);
        // Due by the interval, but the first probe has not finished: nothing starts.
        probes.start_due(start + INTERVAL, &at(30_000), &targets);
        assert_eq!(collect(&mut probes, 1).await[0].at.mono_ms, 0);
        // Give a wrongly started probe time to connect; none may.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(probes.take().is_empty(), "no second probe was started");
        // The API read and the first probe only.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_probe_that_hangs_is_abandoned_and_its_readings_are_unknown() {
        let addr = fake::silent().await;
        let mut probes = Probes::with_timeout(addr, Duration::from_millis(100));
        probes.start_due(
            Instant::now(),
            &at(0),
            &[target("emulator-5584", Some(AdbState::Device))],
        );
        let s = tokio::time::timeout(Duration::from_secs(5), collect(&mut probes, 1))
            .await
            .expect("the probe gives up on its own")
            .remove(0);
        assert_eq!(
            (
                s.boot_completed,
                s.guest_cpu_pct,
                s.guest_mem_available_bytes
            ),
            (None, None, None),
            "unknown, not zero"
        );
        assert_eq!(
            s.adb_state,
            Some(AdbState::Device),
            "adb's own state still stands"
        );
    }

    #[tokio::test]
    async fn no_adb_server_fails_open_with_unknown_readings() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let mut probes = Probes::new(addr);
        probes.start_due(Instant::now(), &at(0), &[target("emulator-5584", None)]);
        let s = collect(&mut probes, 1).await.remove(0);
        assert_eq!(
            (
                s.boot_completed,
                s.guest_cpu_pct,
                s.guest_mem_available_bytes,
                s.adb_state
            ),
            (None, None, None, None)
        );
        assert!(probes.warned, "the first failure is logged once");
    }
}
