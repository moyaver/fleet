// SPDX-License-Identifier: AGPL-3.0-only
//! Invariant I1: the agent never changes the command's exit status or output (PLAN §3, M2 exit).
//! Each test runs the real binary around a small `sh` script standing in for Maestro.

use std::{
    os::unix::process::ExitStatusExt,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn agent() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
}

fn run_sh(script: &str, env: &[(&str, &str)]) -> Output {
    let mut cmd = agent();
    cmd.args(["run", "--finalize-timeout", "3", "--", "sh", "-c", script])
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// What the script does without the agent, for comparison.
fn direct_sh(script: &str) -> Output {
    Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn exit_codes_pass_through() {
    for code in [0, 1, 2, 3, 42, 255] {
        let out = run_sh(&format!("exit {code}"), &[]);
        assert_eq!(out.status.code(), Some(code), "exit {code}");
    }
}

#[test]
fn output_is_byte_exact() {
    // Invalid UTF-8, CR progress, ANSI, NUL, a long line and no final newline, on both streams.
    let script = r"printf '\377\376 bad utf8\r50%%\r100%%\033[31mred\033[0m\000nul'; \
                   head -c 300000 /dev/zero | tr '\0' 'x'; \
                   printf 'err\377' >&2; printf 'tail-without-newline'";
    let wrapped = run_sh(script, &[]);
    let direct = direct_sh(script);
    assert_eq!(wrapped.stdout, direct.stdout, "stdout differs");
    assert_eq!(wrapped.stderr, direct.stderr, "stderr differs");
    assert_eq!(wrapped.status.code(), Some(0));
}

#[test]
fn large_output_passes_through_even_when_the_observer_hangs() {
    let script = "head -c 20000000 /dev/urandom | od -An -tx1 | head -c 20000000";
    let out = run_sh(script, &[("FLEET_FAULT", "observer_hang")]);
    assert_eq!(out.stdout.len(), 20_000_000);
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("did not finish within 3s"), "{stderr}");
}

#[test]
fn observer_failures_never_change_the_result() {
    for fault in ["observer_panic", "observer_abort", "observer_exit"] {
        let out = run_sh("echo out; echo err >&2; exit 7", &[("FLEET_FAULT", fault)]);
        assert_eq!(out.status.code(), Some(7), "{fault}");
        assert_eq!(out.stdout, b"out\n", "{fault}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.starts_with("err\n"), "{fault}: {stderr}");
        assert_eq!(
            stderr.matches("[fleet-agent]").count(),
            1,
            "{fault}: one notice: {stderr}"
        );
    }
}

#[test]
fn a_healthy_observer_adds_nothing_to_the_output() {
    let out = run_sh("echo out; printf 'no newline' >&2; exit 0", &[]);
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"no newline");
}

#[test]
fn signal_deaths_are_re_raised() {
    for (sig, name) in [(15, "TERM"), (9, "KILL"), (1, "HUP")] {
        let out = run_sh(&format!("kill -{name} $$"), &[]);
        assert_eq!(out.status.signal(), Some(sig), "{name}: {:?}", out.status);
    }
}

#[test]
fn core_dumping_signal_deaths_exit_128_plus_n() {
    let out = run_sh("kill -QUIT $$", &[]);
    assert_eq!(out.status.code(), Some(128 + 3), "{:?}", out.status);
}

#[test]
fn missing_and_non_executable_commands() {
    let out = agent()
        .args(["run", "--no-observer", "--", "/nonexistent/maestro"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    let out = agent()
        .args(["run", "--no-observer", "--", "/etc/hosts"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126));
}

/// Starts the agent around `script`, sends `sig` once `ready` appears on stdout, waits.
fn signal_after_ready(
    script: &str,
    sig: &str,
    grace: &str,
) -> (std::process::ExitStatus, Duration, String) {
    let mut child = agent()
        .args([
            "run",
            "--grace",
            grace,
            "--finalize-timeout",
            "3",
            "--",
            "sh",
            "-c",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    while !String::from_utf8_lossy(&seen).contains("ready") {
        let n = std::io::Read::read(&mut stdout, &mut buf).unwrap();
        assert!(n > 0, "script ended before ready");
        seen.extend_from_slice(&buf[..n]);
    }
    let t = Instant::now();
    Command::new("kill")
        .args([format!("-{sig}"), child.id().to_string()])
        .status()
        .unwrap();
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut stdout, &mut rest).unwrap();
    seen.extend_from_slice(&rest);
    let status = child.wait().unwrap();
    (
        status,
        t.elapsed(),
        String::from_utf8_lossy(&seen).into_owned(),
    )
}

#[test]
fn signals_are_forwarded_to_the_process_group() {
    // The child traps TERM and its own background grandchild proves the group was signalled.
    let script = "trap 'echo got-term; exit 143' TERM; (trap 'echo grandchild-term; exit 0' TERM; sleep 30 & wait) & echo ready; wait";
    let (status, took, out) = signal_after_ready(script, "TERM", "10");
    assert_eq!(status.code(), Some(143), "{out}");
    assert!(
        out.contains("got-term") && out.contains("grandchild-term"),
        "{out}"
    );
    assert!(took < Duration::from_secs(5));
}

#[test]
fn a_child_that_ignores_term_is_killed_after_the_grace() {
    let script = "trap '' TERM; echo ready; sleep 30";
    let (status, took, _) = signal_after_ready(script, "TERM", "1");
    assert_eq!(status.signal(), Some(9), "{status:?}");
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(5),
        "{took:?}"
    );
}

#[test]
fn sigint_reaches_the_child() {
    let script = "trap 'exit 130' INT; echo ready; sleep 30 & wait";
    let (status, took, _) = signal_after_ready(script, "INT", "10");
    assert_eq!(status.code(), Some(130));
    assert!(took < Duration::from_secs(5));
}

#[test]
fn a_grandchild_holding_the_pipe_does_not_delay_the_exit() {
    let t = Instant::now();
    let out = run_sh("sleep 30 & exit 4", &[]);
    assert_eq!(out.status.code(), Some(4));
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    thread::sleep(Duration::from_millis(10));
}
