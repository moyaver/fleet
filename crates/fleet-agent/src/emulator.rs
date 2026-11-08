// SPDX-License-Identifier: AGPL-3.0-only
//! Emulator processes (PLAN §6.5 Processes and Devices; research 02 §2; invariant I10).
//!
//! An Android emulator is one `qemu-system-*` process. Its command line names its AVD (`-avd`)
//! and its console port (`-port`), and the console port is the serial's number: port 5554 is
//! `emulator-5554`. The serial-to-process mapping comes from a full process scan, done at start
//! and whenever adb's device list changes. Between scans only the mapped processes are refreshed,
//! so the steady-state cost is one targeted refresh per emulator.
//!
//! Fail-open (I2): a process that cannot be read gives no reading, never a zero (I3, I4).

use std::collections::HashMap;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

/// What an emulator's command line says about its device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The serial adb uses for it, `emulator-<console port>`.
    pub(crate) serial: String,
    /// Its AVD name, when the command line names one.
    pub(crate) avd: Option<String>,
}

/// Reads an emulator's identity from its argument vector (program first). `None` unless the
/// command line names a console port.
pub(crate) fn identify(argv: &[String]) -> Option<Identity> {
    let value_of = |flag: &str| {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let port: u16 = value_of("-port")?.parse().ok()?;
    Some(Identity {
        serial: format!("emulator-{port}"),
        avd: value_of("-avd"),
    })
}

/// One emulator's latest reading.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reading {
    /// Its AVD name, when known.
    pub(crate) avd: Option<String>,
    /// The process's CPU use since its previous refresh, percent of one core.
    pub(crate) cpu_pct: Option<f32>,
    /// The process's resident memory, bytes.
    pub(crate) rss_bytes: Option<u64>,
}

/// A mapped emulator process.
#[derive(Debug)]
struct Running {
    pid: Pid,
    avd: Option<String>,
    cpu_pct: Option<f32>,
    rss_bytes: Option<u64>,
}

/// The emulators Maestro's devices run on, keyed by serial.
#[derive(Debug)]
pub(crate) struct Emulators {
    /// `None` when the platform's process information is unavailable: nothing is mapped.
    system: Option<System>,
    by_serial: HashMap<String, Running>,
}

impl Emulators {
    /// No emulators known yet; call [`rescan`](Self::rescan) to find them.
    pub(crate) fn new() -> Self {
        Self {
            system: System::new_with_specifics(RefreshKind::nothing()).ok(),
            by_serial: HashMap::new(),
        }
    }

    /// Finds the `qemu-system-*` processes and maps each to its serial. A full process scan, so
    /// only at start and when adb's device list changes.
    pub(crate) fn rescan(&mut self) {
        let Some(system) = self.system.as_mut() else {
            return;
        };
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::OnlyIfNotSet),
        );
        self.by_serial = system
            .processes()
            .iter()
            .filter(|(_, p)| p.name().to_string_lossy().starts_with("qemu-system"))
            .filter_map(|(pid, p)| {
                let argv: Vec<String> = p
                    .cmd()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let id = identify(&argv)?;
                let running = Running {
                    pid: *pid,
                    avd: id.avd,
                    cpu_pct: None,
                    rss_bytes: None,
                };
                Some((id.serial, running))
            })
            .collect();
    }

    /// Refreshes the mapped processes only: their CPU since the previous refresh (the scan counts
    /// as the first) and their resident memory.
    pub(crate) fn refresh(&mut self) {
        let Some(system) = self.system.as_mut() else {
            return;
        };
        if self.by_serial.is_empty() {
            return;
        }
        let pids: Vec<Pid> = self.by_serial.values().map(|r| r.pid).collect();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        for running in self.by_serial.values_mut() {
            let process = system.process(running.pid);
            running.cpu_pct = process.map(sysinfo::Process::cpu_usage);
            running.rss_bytes = process.map(sysinfo::Process::memory);
        }
    }

    /// The latest reading for `serial`; `None` when no emulator process maps to it.
    pub(crate) fn reading(&self, serial: &str) -> Option<Reading> {
        self.by_serial.get(serial).map(|r| Reading {
            avd: r.avd.clone(),
            cpu_pct: r.cpu_pct,
            rss_bytes: r.rss_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emulator's own `ps` line, captured on macOS (fixtures/emulator/darwin-26.6, see its
    /// README). The arguments have no spaces, so splitting on whitespace is exact for it.
    const PS_LINE: &str = include_str!("../../../fixtures/emulator/darwin-26.6/ps-qemu-system.txt");

    /// The command line of the captured process: the `ps` columns (pid, parent) are dropped.
    fn captured_argv() -> Vec<String> {
        PS_LINE
            .split_whitespace()
            .skip(2)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_console_port_names_the_serial_and_avd_names_the_device() {
        assert_eq!(
            identify(&captured_argv()),
            Some(Identity {
                serial: "emulator-5590".into(),
                avd: Some("Pixel_10a_atd".into()),
            })
        );
    }

    #[test]
    fn a_command_line_without_a_console_port_maps_nothing() {
        let without_port =
            ["qemu-system-aarch64-headless", "-avd", "Pixel_10a_atd"].map(str::to_owned);
        assert_eq!(identify(&without_port), None);
        let with_bad_port = ["qemu", "-port", "not-a-port"].map(str::to_owned);
        assert_eq!(identify(&with_bad_port), None);
    }

    #[test]
    fn an_avd_flag_is_optional() {
        let argv = ["qemu-system-x86_64", "-port", "5554"].map(str::to_owned);
        assert_eq!(
            identify(&argv),
            Some(Identity {
                serial: "emulator-5554".into(),
                avd: None,
            })
        );
    }

    /// A real process named like an emulator, with a console port: the scan must find it and map its
    /// serial.
    #[cfg(unix)]
    #[test]
    fn a_scan_finds_a_qemu_named_process_and_maps_its_serial() {
        let _emulator = fake::qemu(5599, Some("Test_Device"));
        let mut emulators = Emulators::new();
        emulators.rescan();
        emulators.refresh();
        let found = emulators
            .reading("emulator-5599")
            .expect("the qemu-named process maps to its serial");
        assert_eq!(found.avd.as_deref(), Some("Test_Device"));
        assert!(found.rss_bytes.is_some_and(|b| b > 0));
    }

    #[test]
    fn an_unmapped_serial_has_no_reading() {
        assert_eq!(Emulators::new().reading("emulator-5554"), None);
    }
}

/// Fake emulator processes for tests in this crate.
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use std::{path::PathBuf, process::Child};

    /// A running fake emulator; it is killed and its directory removed when dropped.
    pub(crate) struct FakeEmulator {
        child: Child,
        dir: PathBuf,
    }

    impl Drop for FakeEmulator {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A process named `qemu-system-test` whose command line names console port `port` and, when
    /// given, AVD `avd`: a copy of `sh` that sleeps, with the flags after its script. Its serial is
    /// `emulator-<port>`.
    pub(crate) fn qemu(port: u16, avd: Option<&str>) -> FakeEmulator {
        let dir = std::env::temp_dir().join(format!("fleet-emu-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let program = dir.join("qemu-system-test");
        std::fs::copy("/bin/sh", &program).expect("copy sh");
        let mut args = vec!["-c".to_owned(), "sleep 30".to_owned(), "x".to_owned()];
        args.extend(["-port".to_owned(), port.to_string()]);
        args.extend(
            avd.map(|a| ["-avd".to_owned(), a.to_owned()])
                .into_iter()
                .flatten(),
        );
        let child = std::process::Command::new(&program)
            .args(&args)
            .spawn()
            .expect("the copy should run");
        FakeEmulator { child, dir }
    }
}
