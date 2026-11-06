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
    // The subshell prints `ready` only after its own trap is set: a `ready` printed by the parent
    // would let TERM arrive first, and the untrapped subshell would die silently.
    let script = "trap 'echo got-term; exit 143' TERM; (trap 'echo grandchild-term; exit 0' TERM; echo ready; sleep 30 & wait) & wait";
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

#[test]
fn a_grandchild_that_keeps_writing_does_not_hold_the_exit_past_the_ceiling() {
    // Maestro prints 2000 numbered lines and exits, while a grandchild holding the pipe prints
    // `tick` every 100 ms for about 20 s, longer than the 10 s drain ceiling. The exit must come
    // within the ceiling plus slack, and Maestro's own lines must all be there.
    let script = "(i=0; while [ $i -lt 200 ]; do echo tick; sleep 0.1; i=$((i+1)); done) & seq 1 2000; exit 0";
    let mut child = agent()
        .args(["run", "--no-observer", "--", "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut out).unwrap();
        out
    });
    let t = Instant::now();
    let status = child.wait().unwrap();
    let took = t.elapsed();
    let out = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    assert_eq!(status.code(), Some(0));
    assert!(
        took < Duration::from_secs(13),
        "the exit waited {took:?} for a grandchild that keeps writing"
    );
    // Each tick is one whole write, so removing them leaves exactly Maestro's bytes.
    let maestro = out.replace("tick\n", "");
    let expected: String = (1..=2000)
        .map(|n| format!("{n}\n"))
        .collect::<Vec<_>>()
        .concat();
    assert!(
        maestro == expected,
        "Maestro's output: {} of {} bytes",
        maestro.len(),
        expected.len()
    );
}

/// Runs the agent with its stdout and stderr on non-blocking pipes that slow consumers read, so a
/// full pipe reports `WouldBlock`, as an inherited CI log that fell behind does.
fn run_with_slow_readers(script: &str) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let (out_r, out_w) = nix::unistd::pipe().unwrap();
    let (err_r, err_w) = nix::unistd::pipe().unwrap();
    for w in [&out_w, &err_w] {
        nix::fcntl::fcntl(
            w,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .unwrap();
    }
    let mut child = agent()
        .args(["run", "--finalize-timeout", "3", "--", "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_w))
        .stderr(Stdio::from(err_w))
        .spawn()
        .unwrap();
    // Each consumer stalls for longer than a transient stall once its first bytes arrive, so the
    // pipes fill while the agent is writing, then reads slowly: 16 KiB, then a pause.
    let readers = [out_r, err_r].map(|fd| {
        thread::spawn(move || {
            let mut pipe = std::fs::File::from(fd);
            let mut got = Vec::new();
            let mut buf = [0u8; 16384];
            let mut stalled = false;
            loop {
                match std::io::Read::read(&mut pipe, &mut buf) {
                    Ok(0) => return got,
                    Ok(n) => {
                        got.extend_from_slice(&buf[..n]);
                        let pause = if stalled { 1 } else { 300 };
                        thread::sleep(Duration::from_millis(pause));
                        stalled = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => panic!("read failed: {e}"),
                }
            }
        })
    });
    let status = child.wait().unwrap();
    let [out, err] = readers.map(|r| r.join().unwrap());
    (status, out, err)
}

#[test]
fn a_slow_reader_gets_every_byte_in_order_and_the_exit_status_passes() {
    // About 2 MB per stream: far more than a pipe holds, so both consumers fall behind.
    let script = "seq 1 300000; seq 1 300000 >&2; exit 3";
    let (status, out, err) = run_with_slow_readers(script);
    let expected: Vec<u8> = (1..=300_000)
        .flat_map(|n| format!("{n}\n").into_bytes())
        .collect();
    assert_eq!(status.code(), Some(3));
    assert!(
        out == expected,
        "stdout: {} of {} bytes",
        out.len(),
        expected.len()
    );
    assert!(
        err.starts_with(&expected),
        "stderr: {} of {} bytes",
        err.len(),
        expected.len()
    );
    // Fleet's own notices may follow the output, never interrupt it.
    let notices = &err[expected.len()..];
    assert!(
        notices.is_empty() || notices.starts_with(b"[fleet-agent]"),
        "{}",
        String::from_utf8_lossy(notices)
    );
}

#[test]
fn a_plan_with_flow_paths_is_refused_and_maestro_never_starts() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let plan = dir.join("plan.json");
    std::fs::write(
        &plan,
        r#"{"planId":"p1","totalFlows":1,"shards":[{"id":0,"flows":["a.yaml"]}]}"#,
    )
    .unwrap();
    let marker = dir.join("started");
    let fake = dir.join("maestro");
    std::fs::write(&fake, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = agent()
        .args(["run", "--plan"])
        .arg(&plan)
        .args(["--shard", "0", "--"])
        .arg(&fake)
        .args(["test", ".maestro/"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(".maestro/") && err.contains("Remove the flow paths"),
        "{err}"
    );
    assert!(!marker.exists(), "maestro must not be started");
    std::fs::remove_dir_all(&dir).ok();
}
