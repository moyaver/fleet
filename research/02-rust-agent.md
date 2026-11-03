# 02 — Fleet Agent in Rust: crates, process supervision, architecture

> Research for spec v1.0 §2A, §4, §5, §13 and §15, rewritten for an all-Rust agent.
> Date: 2026-11-03. Crate versions come from the crates.io API on that date.
> The PoC code is throwaway and lives outside the repo: `/tmp/claude-0/-home-user/35b53bb7-a211-5ee3-8f5b-0d3a8129de43/scratchpad/agent-poc/`.

**Evidence markers**

| Marker | Meaning |
|---|---|
| **[M]** | Measured or demonstrated in this PoC (numbers and commands below) |
| **[V]** | Verified by reading crate source, docs.rs or official docs |
| **[D]** | Documented by third parties (blog, issue, forum); not re-verified |
| **[I]** | Inferred or my judgment; needs Phase 0 validation |

The Maestro facts below come from the sibling report [`01-maestro-cli-internals.md`](./01-maestro-cli-internals.md): the launcher `exec`s the JVM, dadb uses the adb server on port 5037, the shutdown hook exists, and there is a `maestro.log` lane. I re-checked none of them against Maestro source; I only cite them.

---

## 0. Executive summary and changes to the spec

**Recommendation.** Write the agent as one static Rust binary with two cooperating processes at runtime:

1. **Supervisor.** This is the PID that CI waits on. It is small, uses only `std`, has no async runtime and no network.
2. **Observer.** A child of the supervisor that holds all the telemetry. It runs on tokio and owns WebSocket, adb, metrics, parsing and uploads.

Maestro runs as a second child of the supervisor in its own process group. The supervisor tees Maestro's bytes to the CI log unchanged, then gives the observer a copy without ever waiting on it. A crash, hang, OOM or abort in the telemetry code cannot change Maestro's exit status or block its output.

The expected footprint is small:

| Item | Measured value |
|---|---|
| Steady-state sampling cost | ~0.02% of one core **[M]** |
| RSS with a live TLS WebSocket | ~10 MB **[M]** |
| Static binary size | 6–7.5 MB **[M]** |

**Changes this implies for spec v1.0:**

| # | Spec location | Change |
|---|---|---|
| 1 | §4 Runtime | Replace Python/psutil/websockets with a single static Rust binary per target: `x86_64`/`aarch64` Linux (musl), `aarch64` macOS. No interpreter on runners. Binary 6.1–7.6 MB stripped, 2–3 MB gzipped **[M]**. |
| 2 | §2A, §4 "Preserve exit" | Add an explicit **supervisor/observer process split** (§1.6). State the invariant: *the process CI waits on never does network I/O and never depends on telemetry code for its exit status.* |
| 3 | §4, §13 | **Fail-open policy.** If config, credentials or the server are bad, still run Maestro (unmonitored) and warn. Only `check`/`selftest` fail hard. Today's spec implies "connect to Fleet Server" is step 3 of `run`. Connecting must not gate step 5 (start Maestro). |
| 4 | §4 exit status | Pass a death by signal through by **re-raising the same signal** after cleanup, not just `exit(128+n)`. It is byte-for-byte transparent to the parent (`WIFSIGNALED`) **[M]**. Fall back to `128+n` for core-dumping signals. |
| 5 | §13 CI cancellation | GitHub Actions gives **7.5 s after SIGINT, then 2.5 s after SIGTERM, then kills the tree** **[D]**. Maestro's shutdown hook wants about 10–15 s (research 01). The agent cannot widen that window. It must therefore send `session.interrupted` **pre-emptively at the first signal**, before Maestro exits, and treat its own cleanup as best-effort. |
| 6 | §4 Devices: "Current activity" | Do **not** sample current activity every 5 s. `dumpsys activity …` is heavy and has been reported to time out on Android 14 **[D]**. Capture it on flow failure and at most every ≥30 s, behind a flag. |
| 7 | §4 Devices | Use the **adb server protocol directly** (`host:track-devices`, push-based) instead of polling `adb devices`. The agent **must never start, restart or kill the adb server**: if nothing listens on 5037, it waits; Maestro/dadb starts the server. Spawning a mismatched `adb` client can restart the server and break Maestro's streams **[I]**. |
| 8 | §4 Metrics | Add **cgroup scope**. Inside Docker/K8s runners report `cpu.max` quota, `cpu.stat` throttling, `memory.max`/`memory.current`, `memory.events` `oom_kill` and cgroup PSI, labelled `scope: "cgroup"`, beside host values (`scope: "host"`). sysinfo covers only cgroup *memory* **[V]**. |
| 9 | §4 Thermal | macOS: `NSProcessInfo.thermalState` (Nominal/Fair/Serious/Critical) is the only supported source. **CPU temperature on Apple Silicon needs private IOHID/SMC interfaces. v1 should always report it as `unavailable` on macOS.** Linux cloud VMs typically expose no thermal zones; this PoC VM had an empty `/sys/class/thermal` and no hwmon **[M]**. |
| 10 | §4 overhead | "No expensive global process scans every second": a full sysinfo process scan costs **~18–21 ms CPU at ~580 tasks** **[M]**. Allow it once at start and on device change. Steady-state sampling refreshes only known PIDs (~0.15 ms) **[M]**. |
| 11 | §4 logging (new) | The agent writes **nothing to stdout**, which belongs to Maestro. On stderr it writes only at line boundaries, normally only a start banner and an end summary. Full logs go to a file next to the artifacts. |
| 12 | §5 Reliability | The bounded queue is an in-memory priority outbox plus an optional **append-only segment spool** (length + CRC32 frames). A post-step `maestro-fleet flush` (`if: always()`) can replay a spool after an agent crash on self-hosted runners. Do not adopt an embedded DB. |
| 13 | §12, §10 | Distribution adds **downloading the agent from Fleet Server itself** (version-matched binaries baked into the server image). Corporate runners already must reach Fleet, and often cannot reach github.com. Ship SHA-256 checksums plus GitHub artifact attestations, plus minisign for offline verification. |
| 14 | §15 | The agent reports its **own** `getrusage` CPU/RSS as `agent.self_metrics` every checkpoint. Overhead then becomes an observable, not just a pre-release benchmark. |

---

## 1. Async runtime and process supervision

### 1.1 Runtime

**Use `tokio` 1.54.1** (`rt-multi-thread` or `current_thread`, `process`, `signal`, `io-util`, `time`, `net`, `fs`, `sync`). It is the only runtime with first-class process, signal, rustls, tungstenite, reqwest and notify integration. It is released continuously; the latest release was 2026-10-21 **[V]**.

Runtime flavour for the observer:

- `current_thread` plus a small blocking pool is enough for a 5 s sampler and one socket.
- The PoC used the default multi-thread runtime: 5 threads and 10.6 MB RSS on 4 cores **[M]**.
- `worker_threads(2)` is a reasonable middle ground.
- sysinfo calls are synchronous `/proc` reads (~1 ms per sample) and can run inline. A `spawn_blocking` hop is not worth it.

The supervisor uses **no async runtime**. Two `std::thread`s pump stdout and stderr, one thread handles signals (`signal-hook`), and the main thread does `waitpid`. Keeping it synchronous makes the "never alter the exit code" core small (~400–600 LOC), easy to audit, and immune to runtime bugs.

### 1.2 Piping and tee, byte-exact

Design rules, all verified in `supervisor` PoC **[M]**:

- **Pipe stdout and stderr separately.** Each pump does `read()` into a 64 KiB buffer, then `write_all()` to the agent's own fd 1/2 *first*, then gives the observer a copy. The copy is a non-blocking write to a pipe (`O_NONBLOCK`). On `EAGAIN` the chunk is dropped and counted, and a `gap{stream, bytes}` marker is sent later. The CI log is never gated on the observer.
- **Never line-buffer the passthrough.** Line splitting happens only on the observer side, so partial lines, `\r` progress updates, ANSI codes, invalid UTF-8 and a missing final newline all pass through unchanged. The PoC compared SHA-256 of direct and wrapped stdout. Input was 2 MB random bytes, invalid UTF-8, CR progress, ANSI, a 300 KB single line and no trailing newline. The hashes were identical, and stderr was identical apart from the agent's own debug lines **[M]**.
- **Bounded line splitter** in the observer:
  - Max line length 16–64 KiB; longer lines are truncated and flagged `truncated: true`.
  - `String::from_utf8_lossy` *only* for parsing and redaction, never for passthrough.
  - Strip ANSI with `strip-ansi-escapes` 0.2.1 (a `vte` wrapper). Research 01 recommends forcing `--no-ansi`; stripping is still a safety net.
  - Treat `\r` as a line break for parsing.
- **Interleaving caveat [I].** Two pipes lose the exact relative order of stdout and stderr writes that a single shared terminal or pipe would show. This shows in CI UIs that merge both streams. Offer `--merge-streams`, which gives the child the same pipe for 2 and 1; the cost is losing stream identity. Default: separate pipes.
- **TTY caveat [I].** A piped child sees `isatty(1) == false`. Maestro then switches to plain output (good for parsing; research 01 §3.1). Local interactive users see plain output instead of the ANSI UI. That is acceptable for v1. A pty mode (`pty-process` 0.5.4, or `nix::pty::openpty`) can come later.
- **SIGPIPE.** Rust ignores SIGPIPE in its own process, so a closed CI stdout gives `EPIPE` rather than killing the supervisor. The pump then keeps *draining* the child so Maestro never blocks. `std::process::Command` restores the default SIGPIPE disposition in the child before `exec`, so Maestro behaves as it would unwrapped **[V]**.
- **Throughput [M].** Piping 1 GiB of 100-byte lines through the PoC (tee plus observer line splitting) took 3.9 s wall versus 0.69 s for direct `> /dev/null`. That is ≈3.4 ms CPU per MiB. The observer copy dropped 44% of bytes under that flood, by design, while passthrough stayed intact. Real Maestro output is KB/s, so this is noise.
- **Startup overhead [M].** Wrapping `/bin/true` added ~6 ms wall and ~3.6 ms CPU.

### 1.3 Process groups and signal forwarding

- Spawn Maestro with `std::os::unix::process::CommandExt::process_group(0)`, which is `setpgid(0,0)` in the child. It is stable `std` and needs no crate.
- Forward SIGINT, SIGTERM, SIGHUP and SIGQUIT with `killpg(pgid, sig)` (`nix` 0.31.3 `sys::signal::killpg`, or `libc::killpg`). This reaches the JVM and any helpers it forked.

PoC results **[M]**:

- SIGTERM to the agent reached the child and its grandchild. The cooperative child exited 143 and the grandchild was gone.
- A child that ignores TERM was escalated to `SIGKILL` of the whole group exactly at the grace deadline (1.5 s configured, 1.52 s observed). Exit 137.

**Why a separate group.** Group-wide forwarding is what tools like GNU `timeout` and `watchexec` do. Without a separate group:

- the agent could only signal the JVM PID;
- terminal Ctrl-C would deliver SIGINT to *both* the agent and Maestro, causing double delivery.

**Consequences of a separate group:**

- **Interactive use [I].** When stdin is a TTY, a background process group reading the terminal gets `SIGTTIN`. Rule: if `isatty(0)`, keep Maestro in the agent's foreground group and forward to the PID only. Otherwise, use a new group. This is the CI case.
- **The CI kills only the agent's group or PID.** Some executors kill only the top PID or its group (GitLab and Jenkins setups vary **[I]**). Maestro's separate group is then not hit. §1.5 handles this.

**Grace and escalation policy:**

- On the first INT/TERM: forward the signal and tell the observer immediately. The observer sends `session.interrupted{phase:"signal_received"}`.
- Start a grace timer: default 7 s on GitHub Actions (so SIGKILL lands before the runner's own SIGTERM at 7.5 s); otherwise 15 s; configurable with `--grace`.
- A second INT/TERM from CI is forwarded again.
- When the grace expires: `killpg(SIGKILL)`.
- Research 01 says Maestro's shutdown hook uninstalls driver APKs over adb and may need more than 7 s. On GitHub this simply may not finish; that is a CI constraint, not an agent bug.

**Crates:**

- Supervisor: `signal-hook` 0.4.6 (iterator on a dedicated thread).
- Observer: `tokio::signal::unix`.
- `signal-hook-tokio` 0.4.0 is unnecessary.
- `process-wrap` 10.0.1 (watchexec) abstracts process group, session and Windows Job Object. It is worth adopting *when Windows is added*; for Unix-only v1, `std` + `nix` is less code.

### 1.4 Exit-code passthrough

| Child outcome | Supervisor action | What the parent sees |
|---|---|---|
| `exit(n)` | `exit(n)` | `n`. Exit 3 passed through **[M]** |
| Killed by signal *s* (not core-dumping) | reset *s* to `SIG_DFL`, unblock it, `raise(s)` | identical to the unwrapped case: Python `returncode=-15`, bash `$?=143` **[M]** |
| Killed by SIGQUIT/SEGV/ABRT/BUS/ILL/FPE | `exit(128+s)` (avoid dumping the agent's core) | `128+s`. SIGKILL → 137 **[M]** |
| Spawn failed: ENOENT | `exit(127)` (shell convention) + `agent.warning` | 127 |
| Spawn failed: EACCES | `exit(126)` | 126 |
| Supervisor itself is SIGKILLed | — (exit status is lost; that is CI's action) | 137 from the CI shell |

Plain `exit(128+s)` is what many wrappers do. Bash reports the same `$?` either way. Re-raising also gives the correct answer to programs that check `WIFSIGNALED`. It also gives the "cooperative SIGINT exit" that makes a calling shell script stop after Ctrl-C. This is the convention documented in "Proper handling of SIGINT/SIGQUIT" (Cracauer).

### 1.5 When the agent itself dies

| Mechanism | Linux | macOS |
|---|---|---|
| `prctl(PR_SET_PDEATHSIG, sig)` in `pre_exec` | Yes. **Fires when the *thread* that forked the child exits, not the process** **[V][M]**. The PoC spawned from a short-lived thread: Maestro was SIGKILLed while the agent was alive, and the run exited 137. **That would turn a passing run into a failure.** Spawn only from the main thread (or another thread that lives as long as the process). Also check `getppid()` after `prctl` against a race. | Not available |
| Coverage | **Direct child only.** The PoC agent was SIGKILLed: the direct child died (left a zombie, because the sandbox's PID 1 does not reap). The grandchild `sleep` survived, reparented to PID 1 **[M]**. Maestro's launcher `exec`s the JVM, so the JVM *is* the direct child (research 01 §9). | — |
| Parent-exit watch from another process | `pidfd_open` + poll, or `getppid()` polling | `kqueue` `EVFILT_PROC`/`NOTE_EXIT` on the supervisor PID (`nix::sys::event` or `kqueue` 1.2.1), or `getppid()` polling |
| Subreaper | `PR_SET_CHILD_SUBREAPER` lets the *living* agent adopt orphaned grandchildren. It does nothing once the agent is dead. | — |

**Recommended combination:**

1. **Linux:** Maestro gets `PDEATHSIG=SIGKILL`, set from the supervisor's main thread. This is the backstop if both agent processes die.
2. **The observer is the watchdog.** It watches the supervisor (Linux: `PR_SET_PDEATHSIG` on the observer + `pidfd`; macOS: `kqueue NOTE_EXIT`, with 1 s `getppid()` polling as a portable fallback). If the supervisor dies while Maestro is alive, the observer:
   - sends `killpg(maestro_pgid, SIGTERM)`, waits for grace, then sends `SIGKILL`;
   - emits `session.interrupted{reason:"agent_supervisor_died"}` best-effort;
   - exits.

   On self-hosted Mac minis this prevents orphan JVMs that would compete with the next job.
3. **Residual risk.** CI SIGKILLs both agent processes but not Maestro's group, on macOS (no PDEATHSIG). Mitigation:
   - the supervisor writes `$RUNNER_TEMP/maestro-fleet/<session>.json` containing `{pgid, start_time}`;
   - an optional post-step `maestro-fleet cleanup` (`if: always()`) kills that group if it still exists *and* the start time matches. The start-time check guards against PID reuse.

   GitHub-hosted runners kill the whole tree anyway **[D]**.

### 1.6 Guaranteeing "never alter Maestro's exit code"

**Options considered:**

| Design | Panic in telemetry | Abort, OOM, stack overflow, segfault in FFI | Telemetry deadlock | Complexity |
|---|---|---|---|---|
| A. Single process, `panic = "abort"` | **kills everything, exit code lost** | kills everything | can wedge exit if finalization awaits it | lowest |
| B. Single process, `panic = "unwind"`. Supervision on the main thread, telemetry on tokio tasks, `catch_unwind` at task roots, panic hook logs. | contained (tokio turns a task panic into `JoinError`) | **kills everything** | bounded by a timeout on finalization | low |
| **C. Supervisor + observer processes (recommended)** | contained | **contained**: the supervisor sees EOF/`SIGCHLD` from the observer, logs one line and carries on | supervisor SIGKILLs the observer at `--finalize-timeout` | moderate (IPC framing ~150 LOC) |

**Design C in detail:**

- The same binary re-execs itself as `maestro-fleet __observe`, using `std::env::current_exe()` (`/proc/self/exe` on Linux).
- It is connected by two pipes:
  - *data* (supervisor → observer): framed output chunks plus control messages `child_started{pid,pgid,argv_redacted,t0}`, `signal{sig}`, `child_exited{status}`, `gap{…}`;
  - *control* (observer → supervisor): `ready`, `finalized`.
- **Supervisor timeline:**
  1. Parse only the flags it needs: `--`, grace, finalize timeout.
  2. Spawn the observer. If that fails, continue unmonitored.
  3. Spawn Maestro.
  4. Tee output.
  5. `waitpid`.
  6. Send `child_exited`.
  7. Wait for `finalized` up to `--finalize-timeout`: default 60 s, plus a separate `--upload-timeout` that the observer enforces.
  8. Exit with Maestro's status.
- **Observer panics** are contained by `panic = "unwind"` (needed anyway for tokio task isolation) plus a panic hook. If the observer dies, the supervisor writes at most one `[maestro-fleet] telemetry stopped: …` line to stderr *at a line boundary*.
- **Supervisor phases** are wrapped in `catch_unwind`. If a tee thread panics, the supervisor still waits for and returns Maestro's status. It then drains the pipe with a plain copy loop.
- **Profile settings:**
  - `panic = "unwind"`. Do **not** use `abort`: it would also apply to the supervisor's code.
  - `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`.
- **Fault-injection hooks for tests:** `FLEET_FAULT=observer_panic|observer_abort|observer_hang|observer_oom|ws_blackhole` (§14).

---

## 2. Host metrics

### 2.1 `sysinfo` 0.40.1

Status: released 2026-10-30, very active, MSRV **1.95** **[V]**.

API notes for 0.40, all verified in source **[V]**:

- `System::new()` and `new_all()` now return `Result<System, Error>`.
- Refresh granularity:
  - `refresh_cpu_specifics(CpuRefreshKind::nothing().with_usage())`;
  - `refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram().with_swap())`;
  - `refresh_processes_specifics(ProcessesToUpdate::Some(&[pids]), remove_dead, ProcessRefreshKind::nothing().with_cpu().with_memory())`. This is the per-process refresh that avoids global scans.
- Static helpers: `System::load_average()`, `System::uptime()`, `System::boot_time()`.
- `System::cgroup_limits()` / `Process::cgroup_limits()` return **memory only**: `total_memory`, `free_memory`, `free_swap`, `rss` from `memory.max`/`memory.current` (v2) or v1 memory. There is **no CPU quota**.
- CPU usage refresh is internally **rate-limited to `MINIMUM_CPU_UPDATE_INTERVAL` (200 ms)**. A naive micro-benchmark measures a no-op (0.1 µs) **[M]**. Process CPU% is computed between two refreshes, so the first sample is 0.
- The default `refresh_processes()` also refreshes `tasks` (threads), disk usage and exe. Always use the `_specifics` form with `.without_tasks()` semantics.

Measured cost on a 4 vCPU x86_64 Linux VM (kernel 6.18). Load: a JVM with 171 threads, 300 idle processes, ~580 tasks total. Release build. Calls spaced to avoid the rate limiter and warm caches **[M]**:

| Call | Cost per call |
|---|---|
| `refresh_cpu_specifics(usage)`, spaced 210 ms | 177 µs wall |
| `refresh_memory_specifics(ram+swap)` | 12–22 µs |
| `load_average()` + `uptime()` | 10–14 µs |
| `cgroup_limits()` | 33–40 µs |
| Raw read of `/proc/loadavg` + `/proc/pressure/{memory,cpu}` + `/proc/uptime` | 21–30 µs |
| `refresh_processes_specifics(Some([jvm, other]), cpu+mem)` | 134–163 µs (JVM alone ≈ 470 µs cold) |
| `refresh_processes_specifics(All, cpu+mem)` | **~18 ms** |
| `refresh_processes(All)` (default incl. tasks) | **~21 ms** |
| `System::new_all()` | **~57 ms** |

Steady state over 60 s each, agent CPU via `getrusage` **[M]**:

| Configuration | Agent CPU | Per sample | RSS |
|---|---|---|---|
| 5 s interval, targeted PIDs + CPU/mem/load/cgroup/PSI | **0.018% of one core** | 0.84 ms | ~3 MB (sampler-only binary) |
| 1 s interval, same | 0.087% | 0.85 ms | ~3 MB |
| 5 s interval + full process scan every sample | 0.49% | 22.7 ms | ~4 MB |
| 5 s interval + full scan every 30 s | 0.13% | 5.9 ms avg | ~4 MB |
| Full agent-like probe: tokio multi-thread, live WSS via proxy, metrics event every 5 s, ping every 15 s, 120 s | **0.016%** | — | **10.6 MB**, 5 threads |

**Conclusion.** Overhead is dominated by how often you do global scans, not by sysinfo. Policy:

- one full scan at start, to find the Maestro JVM tree and emulator `qemu-system-*` processes;
- then targeted refresh of known PIDs;
- re-scan only on a device change event (`track-devices`) or every ≥60 s as a fallback.

### 2.2 What to read directly instead of via a crate

These are cheap text files, ~1–10 µs each **[M]**. A ~200-line module beats a dependency.

| Metric | Linux source | macOS source |
|---|---|---|
| CPU % host, logical cores | sysinfo | sysinfo (host_processor_info) |
| Memory total / available | sysinfo (`MemAvailable`) | sysinfo |
| **Memory pressure** | `/proc/pressure/memory` (`some`/`full` avg10/60/300 + total µs). Needs `CONFIG_PSI`; some kernels boot with `psi=0`, so report `unavailable` if absent. Present in the PoC VM **[M]**. | `sysctl kern.memorystatus_vm_pressure_level` (1 normal, 2 warn, 4 critical) and `kern.memorystatus_level` (% free) via `libc::sysctlbyname`. Type-checked for `aarch64-apple-darwin`, not executed **[M]**. Event-driven alternative: `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` via `dispatch2` 0.3.1 **[I]**. |
| CPU pressure | `/proc/pressure/cpu` | none (use load avg) |
| Swap | sysinfo | sysinfo, or `sysctl vm.swapusage` (`xsw_usage`) |
| Load average | `/proc/loadavg` or `System::load_average()` | `getloadavg` / sysinfo |
| Uptime | `/proc/uptime` | sysinfo (`kern.boottime`) |
| **cgroup v2** (container awareness) | From `/proc/self/cgroup` (`0::/path`) + `/proc/self/mountinfo`: `cpu.max` (`max 100000` or `<quota> <period>` → effective CPUs), `cpu.stat` (`usage_usec`, `nr_throttled`, `throttled_usec` — **throttling is a direct overload signal**), `memory.max`, `memory.current`, `memory.events` (`oom_kill`), `memory.pressure`, `cpu.pressure`, `cpuset.cpus.effective` | n/a |
| cgroup v1 | `cpu.cfs_quota_us`/`cpu.cfs_period_us`, `memory.limit_in_bytes` (sysinfo handles memory). The PoC VM was v1 hybrid, with no `cpu.max` **[M]**. | n/a |

Report both scopes with explicit labels, e.g. `cpu.utilization{scope=host}` and `cpu.quota_cores{scope=cgroup}`, so the dashboard never shows "host 20% busy" while the container is throttled.

### 2.3 Alternatives assessed

| Crate | Version / last release | Verdict |
|---|---|---|
| `heim` | 0.1.0-rc.1, **2020-12** | Abandoned; do not use |
| `systemstat` | 0.2.7, 2026-06 | Maintained but narrower (no per-process); no advantage |
| `psutil` (rust-psutil) | 5.4.0, 2025-07 | Linux/macOS only, less active; sysinfo is the ecosystem default |
| `procfs` | 0.18.0, 2025-08 | Good typed `/proc` (incl. PSI and cgroups helpers). Optional on Linux if hand parsing feels fragile. Linux-only adds a cfg split. |
| `libproc` | 0.14.11, 2025-10 | macOS per-PID info; sysinfo already uses the same APIs |
| `mach2` | 0.7.0, 2026-08 | Raw Mach (host_statistics64); only if sysinfo lacks something |
| `cgroups-rs` | 0.5.1 (Kata) | For *managing* cgroups; heavy; not needed for reading 6 files |

### 2.4 Process attribution

- **Maestro.** The spawned PID *is* the JVM (launcher `exec`s; research 01 §9). Live samples refresh `[jvm_pid]` plus any long-lived children found at the start scan. Short-lived children (`adb`, `avdmanager`) are not worth chasing live. Maestro is the supervisor's direct child, so at exit the supervisor gets exact totals for free from `wait4()` rusage: the CPU of the JVM plus every child it reaped.
- **Emulators:** see §4.4.
- **Agent itself:** `getrusage(RUSAGE_SELF)` for both agent processes, sent as `agent.self_metrics` (§13).

---

## 3. Thermal state

### 3.1 macOS: supported API

`objc2-foundation` 0.3.2 (feature `NSProcessInfo`) exposes `NSProcessInfo::processInfo().thermalState()`. It is a *safe* method returning `NSProcessInfoThermalState(NSInteger)` with constants `Nominal=0`, `Fair=1`, `Serious=2`, `Critical=3` **[V]** (generated source). This compiles for `aarch64-apple-darwin` **[M]** (type-checked only):

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.6.5"
objc2-foundation = { version = "0.3.2", default-features = false, features = ["std", "NSProcessInfo"] }
```

```rust
#[cfg(target_os = "macos")]
pub fn thermal_state() -> ThermalState {
    use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState as T};
    match NSProcessInfo::processInfo().thermalState() {
        T::Nominal => ThermalState::Nominal,
        T::Fair => ThermalState::Fair,
        T::Serious => ThermalState::Serious,
        T::Critical => ThermalState::Critical,
        other => ThermalState::Unknown(other.0),
    }
}
```

- Polling at 5 s is fine; the call is a cheap property read **[I]**.
- `NSProcessInfoThermalStateDidChangeNotification` exists for push, but needs a run loop or notification-centre observer thread. That is not worth it for v1.
- `isLowPowerModeEnabled()` is available too; on Mac minis it is usually false.
- Report `{"thermal_state": "fair", "source": "NSProcessInfo.thermalState", "available": true}`.

### 3.2 Apple Silicon CPU temperature

It is only readable through **private or undocumented interfaces**:

- `IOHIDEventSystemClient*` (private IOKit HID API, loaded via `dlsym`) with vendor usage page `0xFF00`/usage 5 sensors (`tdie*`, `PMU tdev*`);
- or the AppleSMC user client with undocumented four-char keys (`Tp0*`, `Tg0*`). The keys differ between chip generations and even between M1 mini and M1 MacBook Pro.

Tools like `macmon`, `whirr` and SwiftTempBar use these **[D]**. `powermetrics` needs root. **Recommendation:** do not ship this in v1. The spec says "CPU temperature only when an actual sensor is available" and "never invent". Report `cpu_temperature: unavailable, reason: "no_public_api"` on macOS. Revisit only as an opt-in, clearly labelled "experimental, private API" collector.

### 3.3 Linux

- Read `/sys/class/thermal/thermal_zone*/{type,temp}` (millidegrees) and `/sys/class/hwmon/hwmon*/{name,temp*_input,temp*_label}`.
- Prefer hwmon `coretemp` (Intel `Package id 0`), `k10temp` (AMD `Tctl`/`Tdie`), or ARM SoC zones.
- Report source as the file path plus `type`/`name`.
- Throttling hints: `/sys/devices/system/cpu/cpu*/thermal_throttle/*_throttle_count` (Intel) and `cpufreq` `scaling_cur_freq` versus `cpuinfo_max_freq`, as a derived "frequency capped" signal.
- **Cloud VMs** (GitHub-hosted Azure VMs, this Firecracker-style PoC VM) typically expose **nothing**. The PoC VM had `/sys/class/thermal` present but empty and no `/sys/class/hwmon` **[M]**. Model this as `thermal_state: unavailable, reason: "no_sensors_exposed"`.
- `sysinfo::Components` (feature `component`) wraps hwmon and thermal zones. It is an alternative to hand-reading, but adds an enumeration step. Hand-reading ~5 files is simpler and lets us attach provenance.

---

## 4. Android device discovery and health

### 4.1 Options

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Shell out to `adb` | Zero protocol code; matches user expectations | Each call forks a native client (~5–15 ms CPU **[I]**). **A client whose version differs from the running server can kill and restart the server** ("adb server version (x) doesn't match this client (y); killing…") **[I/K]**, which severs Maestro's dadb streams (research 01 §10). Output parsing is fragile. Needs `adb` on PATH (version from *which* SDK?). | Fallback only, and only with the *same* `adb` binary that started the server |
| `adb_client` 3.2.3 (2026-08, single maintainer, ~33k recent downloads) | Pure Rust, implements server and device protocols | **Synchronous** (`std::net::TcpStream`); `track_devices` is a blocking callback loop. Heavy deps (`rsa`, `rcgen`, `rustls`, `image` by default, `num-bigint-dig`, `chrono`). Most of it (USB, push/pull, framebuffer, adbd auth) is irrelevant to us. **[V]** | Not recommended |
| **Own minimal async client for the adb server smart socket on 127.0.0.1:5037 (recommended)** | ~300 LOC on tokio. Request = 4-hex-digit length + ASCII service; reply `OKAY`/`FAIL`+hex length+message **[V]** (AOSP SERVICES.TXT). Push-based device changes via `host:track-devices`. Never starts or kills the server. | We own a small protocol implementation (well documented, stable for >10 years) | **Recommended** |

### 4.2 Services the agent uses

All are read-only:

| Purpose | Service | Notes |
|---|---|---|
| Server alive / version | `host:version` | If the connection is refused: **do not start a server.** Emit `device.status_changed{adb:"server_absent"}` and retry with backoff. Maestro/dadb starts it with `adb start-server` (research 01). |
| **Device list changes** | `host:track-devices` (long-lived) | Server pushes a new full list whenever a device is added/removed or changes state **[V]**. Variants `-proto-binary` / `-proto-text` give richer records (state, product, model, transport id) **[V]**. Whether a `-l` variant exists depends on the platform-tools version; don't rely on it **[I]**. Diff successive lists → `device.discovered` / `device.status_changed`. **No polling.** |
| Per-device shell | `host:transport:<serial>` then `shell:<cmd>` (or `shell,v2,raw:<cmd>` for exit codes) | One TCP connection per command; the connection closes when the command ends |
| Boot readiness, API level, AVD name | **one** `shell:getprop` (all props), parsed once at discovery | `sys.boot_completed=1`, `dev.bootcomplete=1`, `init.svc.bootanim=stopped`; `ro.build.version.sdk`, `ro.build.version.release`; `ro.boot.qemu.avd_name` (modern emulators) **[D]**, falling back to `ro.kernel.qemu.avd_name`; `ro.kernel.qemu`/`ro.boot.qemu` = emulator. Before boot completes, poll `getprop sys.boot_completed` every 2–5 s, then stop. |
| AVD name fallback | `adb emu avd name` = emulator console | Needs a console auth token (`~/.emulator_console_auth_token`); Maestro already runs this itself per enumeration (research 01 §9). Avoid; use getprop. |
| Guest CPU/mem (labelled `guest`) | `shell:cat /proc/stat /proc/meminfo /proc/loadavg` | One cheap exec; compute deltas agent-side. Default every 30 s, not 5 s. Never `dumpsys meminfo`/`top`. |
| Current activity | `shell:dumpsys activity activities` filtered agent-side for `topResumedActivity`/`mResumedActivity` | **Expensive and version-fragile.** Field names change across Android versions; `dumpsys activity top` reported to time out on Android 14 **[D]**. Capture on `flow.failed` and optionally every ≥30 s behind `--device-activity`. Never `uiautomator dump`: Maestro's instrumentation holds UiAutomation (research 01). |

**Cost and risk while Maestro is running [I]:**

- The adb server multiplexes clients. Read-only `getprop`/`cat` adds a few ms of adbd and guest CPU per call.
- Budget: ≤1 shell call per device per 30 s in steady state, plus event-driven calls.
- Per-call timeout 5 s; never more than one in-flight call per device.
- Hard ban: `kill-server`, `reconnect`, `start-server`, `logcat -c`, `pm`, `am force-stop`, `screenrecord`, `uiautomator`.
- Back off automatically, at most one sample per 60 s, if `cpu.pressure`/load is high (spec §4 "may be degraded").

### 4.3 Physical devices and other cases

The same protocol works for USB devices. Without `ro.kernel.qemu` there is no AVD name and no host process. Report `host_process: not_applicable`.

### 4.4 Mapping emulator serial to its host qemu process

Serial `emulator-NNNN` means console port NNNN; adb uses NNNN+1. Resolution order, done once at discovery and cached per serial:

1. **Emulator discovery files.** Each running emulator writes `pid_<PID>.ini` under `…/avd/running/`:
   - Linux: `$XDG_RUNTIME_DIR/avd/running` (else a temp dir);
   - macOS: under `$TMPDIR`-derived paths.

   The file contains `port.serial=5554`, `port.adb=5555`, `grpc.port`, `avd.name`, and so on **[D]** (used by Android Studio tooling and several third-party tools; no official spec). The file name gives the PID directly.
2. **Process scan** (one `ProcessesToUpdate::All` at discovery, ~20 ms). Find `qemu-system-*` / `emulator*` processes. Parse `cmdline` for `-port NNNN`, `-ports NNNN,MMMM` or `-avd <name>` matched against the AVD name from getprop **[I]**.
3. **Socket ownership** (Linux): find the inode listening on `127.0.0.1:NNNN` in `/proc/net/tcp{,6}`, then the PID whose `/proc/<pid>/fd` links to `socket:[inode]`. Costly; last resort.
4. **Unresolvable** (emulator in another container or PID namespace, remote device): `host_process: unavailable`. Never attribute host CPU to a device by guesswork.

Then sample that PID with targeted refresh. Label it `emulator.host_process.cpu` (host CPU %, can exceed 100%), distinct from `guest.cpu` (spec §4: "do not mix").

---

## 5. Watching Maestro output directories

- **`notify` 8.2.1** (2026-10-27): inotify on Linux, FSEvents on macOS (`macos_fsevent` feature; `macos_kqueue` alternative), ReadDirectoryChangesW on Windows **[V]**.
  - `notify` 9.0.0 is at **rc.7**; pin 8.2.x for v1.
  - Debouncer: `notify-debouncer-full` 0.7.0 (0.8 in RC). It coalesces create/modify/rename, tracks file IDs and handles rename pairs.
- **Recursive inotify** adds one watch per directory, walking the tree when the watch is added and automatically for new subdirectories.
  - Maestro ≥2.7 creates one folder per flow with `logs/` and `screenshots/` subfolders (research 01 §4): a few hundred dirs at most, negligible.
  - `fs.inotify.max_user_watches` is RAM-scaled on kernels ≥5.11 (often ≥100k), but can be 8192 on older or tuned hosts and is shared with IDEs and other tools.
  - On `ENOSPC`/`EMFILE`, fall back to `PollWatcher` (2 s) for that root and emit `agent.warning`.
- **FSEvents** is coalescing and path-based. Events can arrive batched with latency (~0.5–1 s typical) and flags merged **[D]**. That is fine because we debounce anyway.
- **Network filesystems / Docker bind mounts from macOS hosts** may not deliver events. A periodic reconciliation scan (`walkdir`, every 10–15 s, cheap for <10k files) catches misses. **A final full inventory scan after Maestro exits is authoritative**; watchers only provide liveness.
- **Write-in-progress files.** Treat a file as complete when:
  - it is a known "last-written" sentinel (e.g. `manifest.json` written at flow end; research 01 §4.3);
  - or its size and mtime are unchanged for 1 s after the last event.

  Parse JSON only after completion. Cap parse size (e.g. 50 MB) and skip with a warning beyond it.
- **Tail `maestro.log`** (research 01's primary live lane):
  - keep the file open, read appended bytes on modify events, plus a 1 s timer fallback;
  - handle truncation and rotation (inode change → reopen);
  - the bounded line splitter is shared with the stdout lane.
- Do not watch `~/.maestro` broadly. Watch only the configured `--debug-output`/`--test-output-dir` roots, which the agent creates if absent (spec step 4).

---

## 6. WebSocket client, TLS, proxy, reconnect

### 6.1 Library choice

| Crate | Version / date | Notes | Verdict |
|---|---|---|---|
| **`tokio-tungstenite`** + `tungstenite` | 0.30.0 / 2026-07 | Most used (~106M recent downloads); `client_async(req, stream)` over *any* `AsyncRead+AsyncWrite`. **No proxy support** and **no automatic ping**. No permessage-deflate. **[V]** | **Recommended** |
| `fastwebsockets` (Deno) | 0.10.0 / 2025-01 | Very fast, low level; handshake via hyper upgrade; throughput irrelevant at <10 msg/s | No benefit |
| `tokio-websockets` | 0.13.3 / 2026-06 | Strict, tokio-util based; good alternative; also no proxy | Acceptable alternative |
| `reqwest-websocket` | 0.6.0 / 2026-01, single maintainer | WS via reqwest's HTTP/1.1 upgrade → **inherits reqwest's proxy, NO_PROXY, TLS and platform verifier for free**; pins `tungstenite` 0.28 / `async-tungstenite` 0.32 **[V]** | Attractive shortcut; bus-factor risk |
| `async-tungstenite` | 0.35.0 | Runtime-agnostic variant | No reason over tokio-tungstenite |

### 6.2 HTTP(S)_PROXY for WebSocket: verified pattern

`tungstenite` maintainers' guidance is to supply your own proxied stream **[D]**. Use `hyper-util` 0.1.21 (`client-proxy` feature):

- `hyper_util::client::proxy::matcher::Matcher::from_env()` reads `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` (and lowercase) with standard semantics.
- `hyper_util::client::legacy::connect::proxy::Tunnel::new(proxy_uri, HttpConnector::new())` performs HTTP `CONNECT`, with optional `with_auth(basic)` **[V]**.
- Then wrap the tunnelled `TcpStream` in `tokio_rustls::TlsConnector` and call `tokio_tungstenite::client_async`.

The PoC connected to `wss://echo.websocket.org/` through the sandbox's HTTP CONNECT proxy. It exchanged ping/pong and text frames, using a glibc build and a static-musl build **[M]**. Match the `wss://` URL as `https://host:port/` for proxy matching. SOCKS5, if ever needed: `tokio-socks` 0.5.3.

### 6.3 TLS and corporate CAs

**Facts:**

- `rustls` 0.23.46 with the `aws-lc-rs` provider (rustls and reqwest 0.13 default).
- `ring` is a lighter-to-build alternative; `aws-lc-rs` offers a FIPS build if IT asks **[V]**.
- **`rustls-platform-verifier` 0.7.1** is reqwest 0.13's default verifier **[V]**:
  - macOS: Security.framework trust evaluation, so MDM/Keychain-installed corporate roots work.
  - Linux: loads roots via `rustls-native-certs` 0.8.5, which honours **`SSL_CERT_FILE` / `SSL_CERT_DIR`** and the OpenSSL default paths.
  - `Verifier::new_with_extra_roots(...)` adds a custom bundle **[V]**.

**PoC findings [M]:**

1. With no readable system roots (`SSL_CERT_FILE=/dev/null`, `SSL_CERT_DIR=/nonexistent`), the verifier errors with "No CA certificates were loaded from the system". **`reqwest::Client::builder().build()` fails outright**, not lazily.
2. Adding `FLEET_CA_FILE` (PEM, parsed with `rustls-pki-types` `PemObject::pem_file_iter`) via `new_with_extra_roots` made WSS succeed again, including from the static musl binary.

**Design:**

- Build **one** `rustls::ClientConfig` at startup: platform verifier + optional `--ca-file`/`FLEET_CA_FILE`.
- Hand the same config to reqwest via `ClientBuilder::tls_backend_preconfigured(...)`/`use_preconfigured_tls`, and to the WS connector, so uploads and the live stream always share trust and proxy settings.
- If the system store is empty *and* no CA file is given, fall back to bundled `webpki-roots` 1.0.11 and say so in `check`.
- Never offer `--insecure` in `run`. A `check --insecure-diagnose` that only *reports* the presented chain is useful for IT.

### 6.4 Keepalive, reconnect, backpressure

- **Ping** every 15 s. Treat it as dead if no frame (pong or any) arrives within 30 s, then reconnect. This stays under the common 60 s idle timeouts (nginx `proxy_read_timeout`, cloud load balancers) **[I]**.
- `TCP_NODELAY` on.
- `WebSocketConfig` with `max_message_size`/`max_frame_size` limits for server→agent frames (e.g. 1 MiB).
- **Reconnect** with `backon` 1.6.0:
  - `ExponentialBuilder` with jitter, 0.5 s → 30 s cap, unlimited attempts while Maestro runs;
  - honour a server `retry_after`.
  - Do **not** use `backoff` 0.4.0: unmaintained since 2021, RUSTSEC-2025-0012 recommends backon **[V]**.
- **Resume handshake** (spec §5): `hello{session_id, agent_version, last_acked_seq}` → server `welcome{server_high_watermark}` → replay events with `seq > watermark` from the outbox, oldest first, lifecycle before metrics.
- **Batching.** Coalesce events into one text frame every ≤250 ms or at 64 KiB. Cuts frame overhead and server wakeups; latency stays well under human perception.
- **Auth.** Bearer token or OIDC-exchanged short-lived token in the `Authorization` header of the upgrade request, built with `IntoClientRequest`. Keep the token in `secrecy::SecretString` 0.10.3 so it never `Debug`-prints.

---

## 7. Durable local buffer for unacknowledged events

### 7.1 Requirements recap

- Bounded memory.
- Lifecycle events preserved preferentially; metrics coalesced first (spec §5).
- Survive WS outages of minutes to hours.
- The agent lives only for the job. On GitHub-hosted runners the disk disappears with the job, so on-disk durability only helps if something replays it. That means an agent crash plus a post-step on the same runner, or self-hosted runners.

### 7.2 Options

| Option | Status | Fit |
|---|---|---|
| `sled` 0.34.7 / 1.0.0-alpha.124 | Last release **2024-10**, perpetual alpha, known space amplification | **Avoid** |
| `redb` 4.4.0 | Active (monthly releases), pure Rust, ACID, stable file format | Correct but overkill: B-tree + MVCC for a FIFO; compaction concerns |
| `rusqlite` 0.41.1 (`bundled`) | Very mature | +~1–1.5 MB and a C build (cross-compile friction); great if we ever need queries |
| **In-memory outbox + append-only segment spool (recommended)** | ~250 LOC + `crc32fast` 1.5.4 | Exactly a FIFO with truncation-by-ack; trivial recovery |

### 7.3 Recommended design

- **Outbox in memory** with three classes:
  - `Lifecycle`: session/suite/flow/step/device/artifact/interrupted; never coalesced.
  - `Metric`: coalescible per series key.
  - `Diagnostic`: `agent.warning`; keep the last N with a counter.
- Budget by bytes, e.g. 8 MiB unacked in memory. That is ≈20k events at 300–500 B.
- **When the memory budget is hit:**
  1. Coalesce metrics: merge adjacent samples of the same series into one `{min,max,avg,count,t_start,t_end}` sample, halving resolution; repeat.
  2. Spill lifecycle and coalesced metrics to the segment spool.
  3. If the spool cap (default 64 MiB) is also hit: drop metrics entirely, then the oldest diagnostics. Only then drop lifecycle events, and emit an `agent.warning{dropped_lifecycle:n}` that the server shows as a gap. Lifecycle events are tiny, so this is very unlikely.
- **Segment spool.**
  - Files `spool/<session>/<first_seq>.seg`, rolled at 4 MiB.
  - Frame = `u32 len | u32 crc32 | payload(JSON)`.
  - On ack, delete whole segments whose last seq ≤ acked.
  - `fdatasync` on roll and at session end, not per event (a power-loss-safe queue is not a goal).
  - Recovery reads frames until the first short or CRC-bad frame (torn tail) and stops.
- **Sequence numbers.** Assigned once by the state actor at creation, `u64` monotonic per agent session. Persisted in the spool so `flush` replays with original seq and the server dedupes (spec §5 idempotency).
- **`maestro-fleet flush --session <id>`.** Optional CI post-step (`if: always()`) that replays a leftover spool. Useful when the observer crashed or the job was cancelled before the outbox drained.

Property tests (§14) cover: no loss of lifecycle events below the cap, monotonic and gapless seq on replay, memory never above budget, and recovery after random truncation.

---

## 8. Artifact upload

- **HTTP client:** `reqwest` 0.13.5 with `default-features = false, features = ["rustls", "stream", "json", "http2", "system-proxy"]`. It uses rustls + aws-lc-rs + platform verifier by default in 0.13, and `tls_certs_merge`/`add_root_certificate` for extra roots **[V]**. Shares the `ClientConfig` with the WS (§6.3).
- **Protocol: prefer presigned PUT.**
  1. Agent → Fleet API: `POST /artifacts/intents {name, kind, size, sha256, content_type}`.
  2. Server applies policy (allowed type, size, quota), dedupes by hash, and returns either `already_have` or a presigned `PUT` URL to S3-compatible storage with short expiry.
  3. Agent streams the file with `reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file))` and an explicit `Content-Length`.
  4. Agent confirms (`POST /artifacts/{id}/complete`).

  Multipart through the API is the fallback for deployments without object-storage presigning. `multipart` with `Part::stream_with_length`.
- **SHA-256:** `sha2` 0.11.1 (+ optional `asm` feature).
  - Hash in a first streaming pass (64 KiB buffer), needed *before* upload for the intent/dedupe and the `x-amz-checksum-sha256` header. Reading a screenshot twice is negligible compared with the network.
  - Note: in sha2 0.11 the digest output type no longer implements `LowerHex`; format it with `base16ct`/`hex`. This tripped the PoC compile **[M]**.
- **Retries:** `backon` with jitter on connect errors, timeouts, 5xx, 408 and 429 (honour `Retry-After`). PUT is idempotent. On 403 from an expired presigned URL, re-request an intent. Max 5 attempts per file.
- **Limits:**
  - per-file max (default 100 MiB; videos configurable), per-session total (default 1 GiB);
  - allowed kinds by extension *and* magic bytes (`infer` 0.22.0);
  - never follow symlinks out of the artifact root (`canonicalize` + prefix check);
  - skip files still being written (§5).
  - If HTML reports are uploaded, mark them `text/plain`/attachment. Server-side sandboxing is research 06's job; the agent only labels.
- **Starvation control:**
  - `tokio::sync::Semaphore` with 2 concurrent uploads.
  - Optional byte-rate cap (token bucket on the stream, `governor` 0.10.4 or a hand-rolled `Interval`), default unlimited after Maestro exits and e.g. 5 MiB/s *while Maestro runs*.
  - During the run upload only small high-value evidence (failure screenshot, `manifest.json`, `commands.json` ≤ 2 MiB). Bulk upload happens after exit, bounded by `--upload-timeout`.
  - Uploads run in the observer, so the supervisor's exit only waits up to `--finalize-timeout`.
- **Redaction before upload** applies to text artifacts (`commands.json`, `maestro.log`); see §11. Binary artifacts are uploaded as-is under the "sensitive" label.

---

## 9. Parsing

| Input | Crate | Approach |
|---|---|---|
| JUnit XML | `quick-xml` 0.42.0 (`serialize`) | Tolerant serde structs (`#[serde(default)]`, `@attr` names, unknown elements ignored). Or use the pull reader (`Reader::read_event_into`) when `system-out` may be huge, with a size cap. `junit-parser` 1.7.0 (built on quick-xml) is an alternative, but Maestro's JUnit has its own attributes (research 01 §5.1), so a small own model is clearer. |
| `commands.json`, `manifest.json` | `serde_json` 1.0.154 | Typed structs with `#[serde(default)]`, plus `#[serde(other)]`/`Value` escape hatches for enums like status, so unknown statuses become `Unknown(String)` instead of a parse error. Version-tag on `manifest.json`'s schema field. |
| Console and `maestro.log` lines | `regex` 1.13.1 | Compile once; anchor on suffix tokens (`RUNNING|COMPLETED|FAILED|SKIPPED|WARNED`, `[Passed]`/`[Failed]`) since flow names are user text (research 01 §14). `RegexSet` for first-pass classification, then the specific regex. |
| Version strings | `semver` 1.0.28 | Parse `Maestro Version:` from the `maestro.log` system-info block (no extra JVM start), else optional `maestro --version` (costs one JVM start ~1–2 s CPU **[I]**), else `--maestro-version` flag, else unknown. |

### Version-aware parser design

```rust
trait LaneParser {                     // one per evidence lane: stdout, maestro.log, artifacts
    fn feed(&mut self, line: &Line, out: &mut Vec<Observation>);
    fn health(&self) -> ParserHealth;  // matched / unknown / anomaly counters
}
struct ParserProfile { versions: semver::VersionReq, build: fn() -> Box<dyn LaneParser> }
static PROFILES: &[ParserProfile] = &[ /* ">=2.7, <2.13" => v2_7, ">=2.0, <2.7" => legacy */ ];
```

- **Selection.** Pick the profile by detected version. If the version is unknown or above the newest tested profile, use the newest profile in **degraded mode**.
- **Degraded mode never emits pass/fail.** It emits:
  - `flow.started`;
  - `flow.completed{outcome: "unknown", evidence: "live_unverified"}`;
  - process lifecycle;
  - artifact discovery.

  Pass/fail then comes only from structured evidence (`manifest.json`, JUnit). Spec §15: "must not silently create false pass/fail".
- **Drift detection.**
  - If, after the first N lines, the expected anchors (plan line, flow start) never matched, switch to degraded and emit `agent.warning{parser_drift}`.
  - If an outcome token was seen but its regex failed to extract a name, switch to degraded.
  - Counters go out in `agent.self_metrics`.
- **Every `Observation` carries provenance:** `lane`, `parser_profile`, `confidence`, `raw_line_hash`. The final reconciliation (JUnit > manifest > console) happens in the state actor, not the parsers.
- **Fixtures** per supported Maestro version (research 01 + `research/fixtures/`) are snapshot-tested (§14). A new Maestro release is "supported" only after its fixtures are recorded.

---

## 10. CLI, config and logging

- **`clap` 4.6.7** (`derive`, `env`).
  - Subcommands: `run`, `check`, `inspect`, `version`, `selftest`, plus hidden `__observe`, `flush`, `cleanup`.
  - `run` uses `#[arg(last = true, required = true)] command: Vec<OsString>` (everything after `--`), so Maestro's arguments are passed through untouched, including non-UTF-8.
- **Config layering:** plain serde + `toml` 1.1.8 + clap `env`. Precedence: CLI flag > `FLEET_*` env > `--config`/`.maestro-fleet.toml` > defaults.
  - Implement as `Option<T>` fields merged in one function. It is ~100 LOC and shows provenance in `check` ("token: from env FLEET_TOKEN").
  - `figment` 0.10.19 does this elegantly but has had no release since **2024-05**.
  - `config` 0.15.27 is active but heavier and stringly-typed.
  - Neither is needed.
- **Logging:** `tracing` 0.1.44 + `tracing-subscriber` 0.3.23 (`env-filter`, `json`) + `tracing-appender` 0.2.5.
  - **stdout is reserved for Maestro's bytes.** The agent never writes to fd 1.
  - **Full agent log goes to a file** `build/fleet/maestro-fleet.log` (or `$RUNNER_TEMP`) via `tracing_appender::non_blocking` with **lossy** mode, so logging can never block a pump or the runtime. It is size-capped (e.g. 10 MiB, then drop with a counter).
  - **stderr:**
    - one start banner (`[maestro-fleet] session … → https://fleet/...`);
    - at most a few warnings (default level `warn`, rate-limited), written **only at a line boundary of Maestro's stderr**: the supervisor's stderr pump tracks "last byte was `\n`" and holds agent lines until then, or until 500 ms of silence;
    - one end summary after Maestro exits.
  - Optional `::warning::` GitHub annotations for "monitoring failed", behind `--gha-annotations` (auto when `GITHUB_ACTIONS=true`).
  - The observer's stderr is a pipe owned by the supervisor (not fd 2 directly), so the line-boundary rule holds for both processes.

---

## 11. Secret redaction

What gets redacted: everything the agent *sends* (events, uploaded text artifacts, the redacted argv). **Never** the CI passthrough, which stays byte-exact; GitHub already masks registered secrets there.

1. **Known-value redaction (most effective).** This is the GitHub Actions masking model. Collect literal values at startup:
   - the Fleet token;
   - values of env vars whose *names* match `(?i)(TOKEN|SECRET|PASSWORD|PASSWD|PWD|API_?KEY|PRIVATE|CREDENTIAL|AUTH|SESSION|COOKIE)`;
   - **all values of Maestro `-e KEY=VALUE` / `--env` arguments.** Flows commonly pass credentials this way, and Maestro may echo `inputText` values and writes them into `commands.json` (research 01 §14 item 10);
   - optional `--redact-env NAME` / `--redact-file`.

   Build one `aho-corasick` 1.1.5 automaton (leftmost-longest). Skip values shorter than 6 chars to avoid shredding logs. Replace with `***`.
2. **Pattern redaction.** A curated `regex::RegexSet` of high-precision patterns:
   - GitHub `gh[pousr]_[A-Za-z0-9]{36}` and `github_pat_…`;
   - AWS `AKIA[0-9A-Z]{16}`;
   - Slack `xox[abprs]-…`;
   - Google `AIza[0-9A-Za-z_-]{35}`;
   - JWT `eyJ[\w-]+\.[\w-]+\.[\w-]+`;
   - PEM `-----BEGIN [A-Z ]*PRIVATE KEY-----`;
   - generic `(?i)(password|secret|token)\s*[:=]\s*\S+`, which redacts the value part.

   Derive the set from gitleaks' `gitleaks.toml`. Its Go RE2 syntax is largely compatible with Rust `regex` (no look-around in either) **[I]**. License believed MIT; keep attribution and verify. `secrets-patterns-db` (1600+ patterns) is noisy for log redaction, and its license is CC-BY / CC-BY-SA 4.0 (sources disagree) **[D]**. Avoid bulk import.
3. **Process args.**
   - Send `argv` as a list with values redacted for `-e/--env K=V` (keep `K`), any `--*token*|--*key*|--*password*|--*secret*` flag value, and URLs with userinfo (`https://user:pass@` → `https://***@`).
   - Never send the environment (spec §10).
   - For *other* processes found by sysinfo (emulator), send only `exe` basename + selected flags (`-avd`, `-port`), never full cmdline.
4. **Streaming correctness.** Redaction runs on complete lines (the splitter), so a secret split across read chunks is still caught. proptest checks this (§14). Values containing newlines: redact each line part ≥6 chars.
5. Mark redaction in events (`redacted: true`, count) so the UI can show "redacted N items".

---

## 12. Packaging and distribution

### 12.1 Build matrix

| Target | How | Result |
|---|---|---|
| `x86_64-unknown-linux-musl` | `cargo-zigbuild` 0.24.0 (zig 0.17 via the PyPI `ziglang` wheel) | **7.4–7.6 MB, statically linked, stripped**; TLS+proxy verified **[M]** |
| `aarch64-unknown-linux-musl` | `cargo zigbuild` cross from x86_64 | **6.1 MB static** **[M]** (built, not executed) |
| `aarch64-apple-darwin` | Native on `macos-14/15` GitHub runners (arm64) | Linker-signed ad-hoc automatically. cargo-dist **refuses** Linux→macOS cross ("a road paved with sadness") **[V]** |
| `x86_64-apple-darwin` | Optional (`lipo` universal binary) | Spec targets arm64 Mac minis only; skip unless Intel Macs exist |

- **Plain `cargo build --target x86_64-unknown-linux-musl` fails** without a musl C toolchain, because `aws-lc-sys`/`ring` contain C ("failed to find tool x86_64-linux-musl-gcc") **[M]**. Hence zigbuild (or `cross` with Docker images).
- `cross` 0.2.5 on crates.io dates from 2023; its users install from git main. zigbuild needs no Docker and is what cargo-dist uses for Linux cross **[V]**.
- **musl allocator.** musl's malloc is slow under contention. Irrelevant at this event rate. If profiling shows otherwise, add `mimalloc` 0.1.52 **[I]**.
- **Clean release build** with fat LTO: ~5–7 min on 4 vCPU, dominated by `aws-lc-sys` and LTO **[M]**. Use `Swatinem/rust-cache`. `ring` instead of `aws-lc-rs` would cut C build time, but reqwest 0.13's `rustls` feature hard-wires aws-lc-rs; use `rustls-no-provider` + ring if desired **[V]**.

### 12.2 Binary size

Full proposed dependency set, x86_64: tokio, tokio-tungstenite, rustls + aws-lc-rs, platform verifier, reqwest, hyper-util, sysinfo, clap, tracing, quick-xml, serde_json, regex, notify, sha2, backon, nix, toml, uuid **[M]**:

| Profile | Size |
|---|---|
| `release` unstripped | 12.0 MB |
| `release` stripped | 9.3 MB |
| `lto="fat"`, `codegen-units=1`, `strip` (glibc) | **7.6 MB** (gzip 3.1 MB) |
| Same + `opt-level="z"` (glibc) | **5.0 MB** (gzip 2.2 MB) |
| Same as row 3, static musl x86_64 | **7.4–7.6 MB** (7.6 MB once the live-session probe code was added) |
| Same as row 3, static musl aarch64 | **6.1 MB** |

`opt-level="z"` is fine for this workload; the CPU-heavy parts are trivial. Ship `"s"` or `"z"` after a quick parse benchmark.

### 12.3 Release tooling: `cargo-dist` ("dist") 0.34.0

Released 2026-11-02, active **[V]**:

- Generates the GitHub Actions release workflow; tarballs per target; `sha256` checksums (`checksum = "sha256"`); shell installer (`installers = ["shell"]`); optional Homebrew tap; `github-attestations = true`; `macos-sign = true` with `CODESIGN_CERTIFICATE`, `CODESIGN_CERTIFICATE_PASSWORD`, `CODESIGN_IDENTITY` secrets **[V]** (config source).
- Notarization is a separate step (`xcrun notarytool submit` of the zip). A bare Mach-O cannot be stapled; Gatekeeper does an online ticket lookup **[D]**.
- Use `cargo-auditable` 0.7.7 to embed the dependency list. Gate releases on `cargo-deny` 0.20.2 (licenses, advisories, duplicate versions).

### 12.4 macOS code signing implications

- Every arm64 Mach-O must be signed. An **ad-hoc linker signature is enough to execute**, and Apple's linker adds it automatically **[D]**.
- Gatekeeper only assesses files with the `com.apple.quarantine` xattr. Browsers, Mail and AirDrop set it; **`curl`, `tar` and our GitHub Action do not** **[D]**. So for CI installs via the action or script, ad-hoc is sufficient.
- If engineers download from a browser, on recent macOS a quarantined, ad-hoc-signed binary is blocked without an easy bypass **[D]**. Either Developer ID-sign + notarize (needs the org's Apple Developer account), or document `xattr -d com.apple.quarantine`.
- **Recommend:** sign + notarize if IT has a Developer ID; otherwise ad-hoc + action/script-only install for Macs.
- Hardened runtime (`codesign -o runtime`) is harmless for this CLI. It needs no special entitlements: no JIT, no debugger.

### 12.5 Integrity and signatures

| Mechanism | Offline-verifiable | Needs | Recommendation |
|---|---|---|---|
| SHA-256 checksums (cargo-dist) | yes | trust in the channel that served the checksum | Always |
| GitHub artifact attestations (Sigstore-backed, keyless) | partly (bundle can be downloaded) | `gh attestation verify --repo org/maestro-fleet` (gh CLI on runner) | Yes for github.com releases. GHES support depends on version **[I]**. |
| cosign keyless | no (Fulcio/Rekor reachability) | Sigstore egress, often blocked in corp networks | Optional |
| **minisign** (Ed25519, `minisign` 0.10.0 pure-Rust or the C tool) | **yes** | public key pinned in the action and install script | **Yes**: simplest air-gap-friendly signature |

**Fleet Server as a distribution channel (new).** Bake the agent binaries for each target into the server image. Serve `GET /api/v1/agent/download?os=linux&arch=arm64&version=…` with the checksum and minisign signature. Runners already need HTTPS to Fleet (spec §10), and the server can pin a compatible agent version.

### 12.6 GitHub Action wrapper

A composite action `maestro-fleet/setup` (bash; Windows later via pwsh):

1. Map `runner.os`/`runner.arch` → target triple. Linux always uses musl, so glibc age doesn't matter.
2. Resolve the version input (`latest`, a semver, or `server` = ask Fleet Server).
3. Download from the GitHub release *or* Fleet Server URL.
4. Verify SHA-256 + minisign (built-in public key). Optionally run `gh attestation verify` when `verify-attestation: true`.
5. Cache in `$RUNNER_TOOL_CACHE/maestro-fleet/<ver>/<arch>`.
6. Add to `GITHUB_PATH`.
7. Output `version`.

Optionally pair it with a post-step (`if: always()`) running `maestro-fleet cleanup && maestro-fleet flush` (§1.5, §7.3). JS actions get a real `post:` hook. With composite actions, document the explicit `if: always()` step.

---

## 13. Overhead: expectations and measurement

**Expected agent overhead** (from §2.1 measurements on a 4 vCPU VM; Apple Silicon should be similar or lower **[I]**):

| Component | CPU | Memory |
|---|---|---|
| Supervisor (tee threads idle-waiting in `read`) | ≈0; 3.4 ms CPU/MiB of output **[M]** | ~1–2 MB RSS **[I]** |
| Observer: sampling @5 s, WSS keepalive, metrics events | ~0.02% of one core **[M]** | ~10 MB RSS with multi-thread runtime **[M]**; expect 6–8 MB with `current_thread` **[I]** |
| Device probes (1 shell per device per 30 s) | adb server + adbd + guest cost, not ours: a few ms per call **[I]** | — |
| One-time start scan (all processes) | ~20 ms at 580 tasks **[M]** | — |
| Upload after exit | network-bound; SHA-256 ≈ 0.5–2 GB/s per core **[I]** | 64 KiB buffers × 2 |

Target from spec §4 "<1% average host CPU": met by about 50× on the measured host.

**How to measure (spec §15):**

1. **Self-accounting in production.**
   - Both agent processes publish `getrusage(RUSAGE_SELF)` deltas (user+sys µs, `ru_maxrss`) and current RSS (`/proc/self/statm` / `task_info` on macOS) every checkpoint (30–60 s) as `agent.self_metrics`.
   - The supervisor reports Maestro's `wait4` rusage at exit.
   - Dashboards can then show "agent overhead = x% of job CPU" for every job, not just in a benchmark.
2. **A/B benchmark in CI.**
   - Same suite, same runner class, N ≥ 15 runs each, interleaved order (A B A B …), with and without `maestro-fleet run`.
   - Compare wall time and Maestro JVM CPU with a bootstrap CI on the median.
   - CI noise is typically several percent, so the honest claim is "no detectable difference at ±x%", plus the self-accounted CPU number.
3. **Micro-benchmarks.**
   - `hyperfine 'maestro-fleet run -- true'` (startup);
   - `criterion` benches for line splitter, redactor and parsers over recorded fixtures (e.g. ≥200 MB/s target);
   - `perf stat -p` / `pidstat -p` (Linux) and `/usr/bin/time -l` (macOS) for whole-run checks.
4. **Stress.** Use the flood fake (1 GiB output, §14):
   - assert passthrough integrity (hash);
   - assert observer RSS stays under budget;
   - assert dropped-copy counters are reported.

---

## 14. Testing strategy (agent-specific)

- **`fake-maestro` harness.** A small Rust binary in the workspace (`tests/fake-maestro`) driven by a script file (TOML/JSON).
  - Steps:
    - `stdout`/`stderr` chunk with delay (allows mid-line splits, invalid UTF-8, `\r`, ANSI);
    - `write_file`/`copy_fixture` into the output dir at time T (simulates `manifest.json` appearing);
    - `spawn_grandchild`;
    - `on_signal: ignore|exit(n)|delay(ms)`;
    - `exit(n)` / `raise(sig)` / `abort` / `hang`.
  - Record real runs with a hidden `maestro-fleet __record -- maestro test …`, which stores chunk timings plus an artifact timeline. This turns research 01's fixtures into replayable sessions.
  - The PoC scripts (`binary.sh`, `trap.sh`, `stubborn.sh`, `selfkill.sh`, `flood.sh`) are the seed cases.
- **Exit/signal tests** (`assert_cmd` 2.2.2, `nix::sys::signal::kill`, run under **`cargo-nextest`** 0.9.151 for per-test process isolation and timeouts). Assert:
  - every exit-code row in §1.4;
  - forward-to-group with grandchild death;
  - escalation timing within ±200 ms of the grace;
  - PDEATHSIG backstop;
  - observer-watchdog kill when the supervisor is SIGKILLed;
  - re-raise semantics (`ExitStatusExt::signal()` equals the child's).
- **Fault injection.**
  - `FLEET_FAULT=observer_panic|observer_abort|observer_hang|observer_oom|ws_blackhole|disk_full`.
  - For each: passthrough hash unchanged, exit status unchanged, supervisor exit within `finalize_timeout + 1 s`.
  - This is the executable form of acceptance criterion 11.
- **Snapshot tests: `insta` 1.49.0** (`yaml`/`json` features, redactions for timestamps and IDs). Inputs are fixture logs, JUnit and `commands.json`. Snapshot the `Observation` stream and the reconciled final state. Review drift with `cargo insta review`.
- **Property tests: `proptest` 1.12.0.**
  - Line splitter: any chunking of the same bytes yields the same lines; no line exceeds the cap.
  - Redactor: an injected secret never appears in the output under any chunking.
  - Outbox: random sequences of enqueue/ack/disconnect/replay keep lifecycle events, seq stays strictly increasing, and memory stays bounded.
  - Spool: truncation at a random byte offset recovers a valid prefix.
  - Protocol: serialize→deserialize round-trips; server-side dedupe by `(session, seq)`.
- **Fake Fleet server** in tests: `tokio-tungstenite` `accept_async` with a scripted behaviour (ack lag, drop connection at seq N, duplicate acks, refuse auth, slow pong), plus `wiremock` 0.6.5 for upload endpoints (5xx then success, 403-expired presign, slow body). Clock skew: inject a `Clock` trait in the state actor.
- **Platform CI matrix:** `ubuntu-24.04` (x86_64), `ubuntu-24.04-arm`, `macos-15` (arm64), plus a Docker job with `--cpus=1 --memory=512m` (cgroup v2 path) and a Docker job with an empty CA store (TLS fallback path). Emulator integration (Phase 0/1) runs on the Linux KVM runners.

---

## 15. Proposed internal architecture

### 15.1 Processes

```
 CI step shell
   │ fork/exec; waits on this PID (exit status = Maestro's)
   ▼
┌──────────────────────────────── P1  maestro-fleet run  (SUPERVISOR) ─────────────────────────────────┐
│ std only · no tokio · no network · no parsing                                                        │
│                                                                                                      │
│  [sig thread]  signal-hook: INT TERM HUP QUIT ──► killpg(C) ; notify P2 ; grace timer ──► SIGKILL(C)  │
│  [tee-out]     read(C.stdout 64K) ──► write_all(fd1, CI log) ──► try_write(dpipe) ─╳ EAGAIN: count  │
│  [tee-err]     read(C.stderr 64K) ──► write_all(fd2, CI log) ──► try_write(dpipe) ─╳ EAGAIN: count  │
│  [main]        spawn P2 (optional) → spawn C → waitpid(C) → send child_exited → wait P2 ≤ T_final    │
│                → exit(code) | re-raise(sig)                                                          │
└──────┬────────────────────────────────────────────────────────────┬──────────────────────────────────┘
       │ spawn: new pgid, PDEATHSIG=SIGKILL (Linux),                 │ spawn: same pgid as P1,
       │ stdin inherited, stdout/err piped                           │ dpipe (data, nonblocking writer)
       ▼                                                             │ cpipe (control)
  C  maestro → exec java (JVM) ──► dadb ──► adb server :5037         ▼
                                            ▲              P2  maestro-fleet __observe  (OBSERVER, tokio)
                                            │                watches P1: PDEATHSIG+pidfd / kqueue NOTE_EXIT
                                            │                on P1 death → killpg(C) TERM→KILL, emit interrupted
                                            └──────── read-only smart-socket client (see below)
```

### 15.2 Observer actors and channels

All channels are bounded. No actor ever awaits the network while holding input from the supervisor.

```
 dpipe ─► [ingest] ─ frame decode ─ line split (64K cap) ─ ANSI strip ─ redact ─┐
                                                                                 │ mpsc(1024) Observation
 maestro.log ─► [log tailer] (notify + 1s timer) ─ line split ─ redact ─────────┤
                                                                                 │
 output dirs ─► [fs watcher] notify + debouncer + 15s reconcile ─ artifact cls ─┤
                                                                                 ▼
 adb :5037 ─► [device mgr] track-devices stream; per-device probe tasks ──► [STATE ACTOR] ◄── cpipe
              (≤1 in-flight per device, 5s timeout)                          single owner of session
                                                                              state machine, provenance,
 [metrics] interval 5s (MissedTickBehavior::Skip) ─ sysinfo targeted ─────►  reconciliation, seq numbers
           + /proc, cgroup, PSI, thermal; auto-throttle on pressure          │            │
                                                                               │ events     │ upload jobs
                                                                               ▼            ▼
                                            [OUTBOX] priority queues + byte budget    [uploader]
                                             + coalescer + segment spool              Semaphore(2), rate cap,
                                               │  ▲ ack(seq)                          sha256 → intent → PUT
                                               ▼  │                                   (reqwest, shared TLS)
                                           [transport] WSS via proxy Tunnel + rustls,
                                            hello/resume, batch ≤250ms, ping 15s/30s,
                                            backon reconnect
```

**Rules that guarantee the passthrough path never blocks on network:**

1. The passthrough write (`write_all` to fd 1/2) happens in P1, before any copy, in a process with no sockets.
2. P1 → P2 is a non-blocking pipe write; a full pipe drops the *copy*, never delays the read loop. Raise the pipe size with `F_SETPIPE_SZ` (Linux, up to 1 MiB) to make drops rarer.
3. Inside P2, ingest → state uses bounded `mpsc`. If state is slow, ingest blocks, which only fills P2's pipe, see rule 2.
4. State → outbox is a synchronous in-memory push. The outbox never awaits the socket; coalescing and spilling enforce its bounds.
5. The transport is the only actor that awaits the network, and it only *pulls* from the outbox.

### 15.3 Lifecycle and finalization

```
start ─► P1 parses minimal flags ─► spawn P2 (P2 loads config; on error → "unmonitored" warning, P1 continues)
      ─► spawn C immediately (do NOT wait for WS connect) ─► P2 connects in background, events buffer meanwhile
signal ─► P1 forwards; P2 sends session.interrupted{phase:signal_received} with priority-flush
C exits ─► P1: child_exited{status, rusage} ─► P2: drain ingest, fs quiesce (1s), final inventory scan,
          parse JUnit, reconcile, suite/session.completed, upload (≤ upload_timeout), flush outbox (≤ 10s)
      ─► P2: finalized ─► P1 exits with C's status   (P1 kills P2 at finalize_timeout regardless)
```

---

## 16. Recommended crate list

Versions are the latest stable on crates.io as of 2026-11-03.

| Purpose | Crate | Version | Maintenance (last release) | Notes |
|---|---|---|---|---|
| Async runtime (observer) | `tokio` | 1.54.1 | Very active (2026-10-21) | `process`, `signal`, `net`, `time`, `fs`, `io-util` |
| Codec utils | `tokio-util` | 0.7.20 | Active (2026-10) | `ReaderStream` for uploads |
| Byte buffers | `bytes` | 1.12.1 | Active | |
| Process groups / spawn | `std` (`CommandExt::process_group`, `pre_exec`) | Rust ≥1.64 | — | No crate needed on Unix |
| Signals (supervisor) | `signal-hook` | 0.4.6 | Active (2026-10-31) | Iterator on a thread |
| POSIX calls | `nix` | 0.31.3 | Active (2026-05) | `killpg`, `kqueue` (`event`), `pidfd` alternative via `libc` |
| Raw libc | `libc` | 0.2.191 | Active | `prctl`, `sysctlbyname`, `getrusage` |
| Future Windows process mgmt | `process-wrap` | 10.0.1 | Active (watchexec, 2026-09) | Process group/session/Job Object/KillOnDrop |
| Host metrics | `sysinfo` | 0.40.1 | Very active (2026-10-30); **MSRV 1.95** | `default-features=false, features=["system"]`; targeted refresh only |
| Linux /proc typed (optional) | `procfs` | 0.18.0 | Maintained (2025-08) | Only if hand parsing PSI/cgroup is undesired |
| macOS thermal | `objc2-foundation` (+`objc2`) | 0.3.2 (+0.6.5) | Active (objc2 2026-10) | Feature `NSProcessInfo`; safe `thermalState()` |
| macOS memory pressure (event) | `dispatch2` | 0.3.1 | Active (objc2 family) | Optional; sysctl polling suffices |
| ANSI stripping | `strip-ansi-escapes` | 0.2.1 | Stable, low churn (2025-01) | `vte`-based |
| adb | **own client** over `tokio::net::TcpStream` | — | — | `adb_client` 3.2.3 rejected (sync, heavy) |
| File watching | `notify` | 8.2.1 | Active (2026-10-27); 9.0 in RC | `macos_fsevent` |
| Debounce | `notify-debouncer-full` | 0.7.0 | Active (2026-05; 0.8 RC) | |
| Dir walk / globs | `walkdir` / `globset` | 2.5.0 / 0.4.21 | Stable / active | Final inventory, artifact filters |
| WebSocket | `tokio-tungstenite` | 0.30.0 | Active (2026-07) | No proxy, no auto-ping: handled by us |
| Proxy CONNECT + env matching | `hyper-util` | 0.1.21 | Very active (hyperium) | `client-proxy` features: `Matcher::from_env`, `Tunnel` |
| TLS | `rustls` / `tokio-rustls` | 0.23.46 / 0.26.6 | Very active | `aws-lc-rs` provider (default) or `ring` |
| OS trust store | `rustls-platform-verifier` | 0.7.1 | Very active (rustls org) | macOS Keychain; Linux via native-certs (`SSL_CERT_FILE/DIR`) |
| Fallback roots | `webpki-roots` | 1.0.11 | Active | Only when the system store is empty |
| PEM parsing | `rustls-pki-types` | 1.15.1 | Active | `PemObject::pem_file_iter` (replaces `rustls-pemfile`) |
| Retry/backoff | `backon` | 1.6.0 | Maintained (2025-10) | `backoff` 0.4 unmaintained (RUSTSEC-2025-0012) |
| HTTP uploads | `reqwest` | 0.13.5 | Very active (2026-09) | rustls + platform verifier default; `stream`, `json` |
| Hashing | `sha2` | 0.11.1 | Active (RustCrypto) | Output no longer `LowerHex`; use `base16ct`/`hex` |
| File type sniffing | `infer` | 0.22.0 | Active | Upload validation |
| Spool checksums | `crc32fast` | 1.5.4 | Active | Segment frames |
| XML (JUnit) | `quick-xml` | 0.42.0 | Very active | `serialize` |
| JSON | `serde` / `serde_json` | 1.0.229 / 1.0.154 | Very active | Tolerant structs |
| Regex / multi-literal | `regex` / `aho-corasick` | 1.13.1 / 1.1.5 | Very active | Parsing, redaction |
| Versions | `semver` | 1.0.28 | Stable | Parser profile selection |
| CLI | `clap` | 4.6.7 | Very active | `derive`, `env` |
| Config file | `toml` | 1.1.8 | Active | Plain serde layering (no figment/config) |
| Logging | `tracing` / `tracing-subscriber` / `tracing-appender` | 0.1.44 / 0.3.23 / 0.2.5 | Active | Lossy non-blocking file writer |
| Secrets in memory | `secrecy` | 0.10.3 | Stable (2024-10) | Token handling |
| IDs / time | `uuid` (v7) / `jiff` | 1.29.0 / 0.2.39 | Active | Time-ordered event/session IDs; RFC 3339 |
| Errors | `anyhow` / `thiserror` | 1.0.104 / 2.0.21 | Active | |
| Test: CLI | `assert_cmd` | 2.2.2 | Active | |
| Test: snapshots | `insta` | 1.49.0 | Very active | |
| Test: properties | `proptest` | 1.12.0 | Active | |
| Test: HTTP mocks | `wiremock` | 0.6.5 | Maintained (2025-08) | |
| Test runner | `cargo-nextest` | 0.9.151 | Very active | Process-per-test |
| Cross builds | `cargo-zigbuild` | 0.24.0 | Active (2026-10) | Linux musl x86_64/aarch64 |
| Release | `cargo-dist` | 0.34.0 | Active (2026-11-02) | Checksums, installers, attestations, macOS signing |
| Supply chain | `cargo-deny` / `cargo-auditable` | 0.20.2 / 0.7.7 | Active | |
| Signatures | `minisign` (tool or crate) | 0.10.0 | Active | Offline verification |

**Rejected or avoided:**

- `heim`: abandoned 2020.
- `sled`: stalled alpha.
- `backoff`: unmaintained.
- `adb_client`: sync and heavy for this use.
- `fastwebsockets`: no benefit at this rate.
- `figment`: quiet since 2024; not needed.
- `cross`: stale crates.io release; zigbuild is simpler.

---

## 17. Cost of adding Windows later [I]

The design maps cleanly, but a few pieces are Unix-specific. Estimate: **2–4 engineer-weeks** including CI and signing.

| Area | Windows equivalent | Effort |
|---|---|---|
| Process group + forwarding | Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Stronger than PDEATHSIG: the tree dies when the last handle closes. Console Ctrl-C/Break via `GenerateConsoleCtrlEvent` with `CREATE_NEW_PROCESS_GROUP`. `process-wrap` covers both. | Medium |
| Exit status | No signals; pass the `DWORD` exit code. `0xC000013A` = Ctrl-C. | Low |
| Supervisor/observer pipes | Anonymous pipes. Non-blocking writes need overlapped I/O or a bounded writer thread with drop-on-full. | Medium |
| Metrics | sysinfo works (no load average: it returns `Unsupported` in 0.40.1 **[V]**). No PSI; memory pressure via `GlobalMemoryStatusEx` load %. | Low |
| Thermal | No public, reliable API (WMI `MSAcpi_ThermalZoneTemperature` is often absent or admin-only). Mark unavailable. | Low |
| File watching | `notify` (ReadDirectoryChangesW) works; path-length and locking quirks | Low |
| adb | Same TCP protocol | None |
| TLS/proxy | Platform verifier uses the Windows cert store; WinHTTP/IE proxy settings via `hyper-util` `from_system` | Low |
| Packaging | MSVC target on Windows runners; Authenticode signing (cargo-dist supports SSL.com/Azure signing); PowerShell installer; `pwsh` action branch | Medium |
| Console encoding | Maestro's output code page (UTF-8 vs ANSI CP); keep passthrough byte-exact | Low |

---

## 18. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| PDEATHSIG fires from a dying thread and kills a healthy Maestro | Medium if spawning from a pool thread | **Turns pass into fail** (demonstrated **[M]**) | Spawn only from the supervisor main thread; test asserts it |
| Agent bug alters exit code | Low with design C | Critical (spec §16 #11, #15) | Two-process split, std-only supervisor, fault-injection suite |
| CI cancel window (GitHub 7.5 s + 2.5 s) shorter than Maestro shutdown hook | High on GitHub | Interrupted runs lack cleanup/evidence | Pre-emptive `session.interrupted`; grace ≤7 s on GHA; server marks stale; never fabricate results |
| Agent's adb usage disturbs Maestro (server restart, heavy dumpsys) | Low–medium | Flaky test failures blamed on Maestro | Own protocol client; never start/kill server; strict call budget; `dumpsys` only on failure |
| Corporate TLS inspection / no system CA store in containers | Medium | No telemetry (tests unaffected) | Platform verifier + `FLEET_CA_FILE` + webpki fallback; `check` reports the chain |
| Proxy requires NTLM/Kerberos auth | Low–medium | No telemetry | Basic auth supported via Tunnel; document NO_PROXY/allowlisting Fleet; NTLM out of scope **[I]** |
| Maestro output/artifact format changes (every 3–5 weeks per research 01) | High | Wrong live progress | Version-gated profiles, degraded mode never emits pass/fail, drift detection, fixture CI on each Maestro release |
| sysinfo MSRV (1.95) and API churn (0.x, breaking changes each minor) | Medium | Build breaks on upgrade | Pin minor; wrap behind a `HostSampler` trait; rust-toolchain pinned ≥1.95 |
| inotify watch exhaustion on busy self-hosted hosts | Low | Missed live artifact events | PollWatcher fallback + periodic reconcile + final scan |
| Unsigned/unnotarized macOS binary downloaded via browser | Medium for humans, low for CI | Blocked execution | Developer ID + notarization if available; action/script install path |
| Secret leakage via `commands.json`/logs | Medium | Security incident | Known-value + pattern redaction before upload; argv redaction; mark sensitive |
| aws-lc-sys build friction (C toolchain, cross) | Medium | Release pipeline pain | zigbuild verified; `ring` fallback via `rustls-no-provider` |
| Orphan Maestro on macOS if both agent processes are SIGKILLed | Low | Resource contention on next job | `cleanup` post-step with pgid + start-time pidfile |

---

## 19. What the PoC did and did not cover

**Verified [M]:**

- byte-exact tee with pathological output;
- exit-code passthrough incl. 137;
- re-raise transparency;
- group forwarding and grace escalation;
- PDEATHSIG scope (direct child only) and the thread pitfall;
- sysinfo costs and steady-state overhead;
- WSS via HTTP CONNECT proxy with platform verifier and extra CA, glibc and static musl;
- binary sizes for 4 profiles and 2 musl targets;
- zigbuild cross-compile to aarch64;
- macOS thermal snippet type-checks.

**Not verified (needs Phase 0 hardware):**

- execution on macOS and arm64 Linux;
- `thermalState` values on a hot Mac mini;
- adb protocol calls against a real emulator while Maestro runs, and their guest cost;
- `avd/running` discovery file paths and fields on current emulator versions;
- FSEvents latency on the Maestro output tree;
- GitHub-hosted runner thermal and PSI exposure;
- `current_thread` RSS figure.

**Test environment:** 4 vCPU x86_64 VM, Linux 6.18, 16 GB, cgroup v1 hybrid, Rust 1.97.0, zig 0.17.0. Load: JVM (Java, 171 threads) + 300 idle processes, ~580 tasks.

---

## Sources

**Crates and versions** (crates.io API, 2026-11-03):

- https://crates.io/crates/tokio, https://crates.io/crates/sysinfo, https://crates.io/crates/heim, https://crates.io/crates/systemstat, https://crates.io/crates/procfs, https://crates.io/crates/psutil
- https://crates.io/crates/adb_client, https://crates.io/crates/notify, https://crates.io/crates/notify-debouncer-full
- https://crates.io/crates/tokio-tungstenite, https://crates.io/crates/fastwebsockets, https://crates.io/crates/tokio-websockets, https://crates.io/crates/reqwest-websocket
- https://crates.io/crates/redb, https://crates.io/crates/sled, https://crates.io/crates/rusqlite
- https://crates.io/crates/backon, https://crates.io/crates/backoff, https://crates.io/crates/figment, https://crates.io/crates/config
- https://crates.io/crates/cargo-dist, https://crates.io/crates/cargo-zigbuild, https://crates.io/crates/cross, https://crates.io/crates/process-wrap

**Crate source and docs read for API verification:**

- sysinfo 0.40.1 `src/common/system.rs`, `src/unix/linux/cgroup.rs`, `src/unix/linux/cpu.rs`, CHANGELOG: https://docs.rs/sysinfo/0.40.1/sysinfo/
- objc2-foundation 0.3.2 `src/generated/NSProcessInfo.rs`: https://docs.rs/objc2-foundation/latest/objc2_foundation/struct.NSProcessInfo.html, https://docs.rs/objc2-foundation/latest/objc2_foundation/struct.NSProcessInfoThermalState.html
- rustls-platform-verifier 0.7.1 `src/verification/others.rs`, `src/lib.rs`: https://docs.rs/rustls-platform-verifier/0.7.1/
- rustls-native-certs 0.8.5 (`SSL_CERT_FILE`/`SSL_CERT_DIR`): https://docs.rs/rustls-native-certs/0.8.5/
- reqwest 0.13.5 `Cargo.toml` features, `async_impl/client.rs`: https://docs.rs/reqwest/0.13.5/
- hyper-util 0.1.21 `client/proxy/matcher.rs`, `client/legacy/connect/proxy/tunnel.rs`: https://docs.rs/hyper-util/0.1.21/
- tokio-tungstenite 0.30.0 features and API: https://docs.rs/tokio-tungstenite/0.30.0/
- adb_client 3.2.3 (`server/commands/devices.rs`, Cargo features): https://docs.rs/adb_client/3.2.3/
- reqwest-websocket 0.6.0: https://docs.rs/reqwest-websocket/0.6.0/
- cargo-dist 0.34.0 (`src/tasks.rs` cross-compile policy, `src/config/v0.rs` `github_attestations`, `macos_sign`, checksum): https://docs.rs/cargo-dist/0.34.0/ and https://axodotdev.github.io/cargo-dist/
- cargo-dist macOS signing source: https://docs.rs/crate/cargo-dist/0.32.0/source/src/sign/macos.rs

**Platform behaviour:**

- prctl(2) `PR_SET_PDEATHSIG` (thread semantics): https://man7.org/linux/man-pages/man2/prctl.2.html; https://manpages.opensuse.org/Tumbleweed/man-pages/PR_SET_PDEATHSIG.2const.en.html; kernel bug 43300: https://bugzilla.kernel.org/show_bug.cgi?id=43300
- Proper handling of SIGINT/SIGQUIT (cooperative exit): https://www.cons.org/cracauer/sigint.html
- Linux PSI: https://docs.kernel.org/accounting/psi.html; cgroup v2: https://docs.kernel.org/admin-guide/cgroup-v2.html
- GitHub Actions cancellation (SIGINT → 7.5 s → SIGTERM → 2.5 s → kill tree): https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-cancellation; observations: https://github.com/ringerc/github-actions-signal-handling-demo
- AOSP adb SERVICES.TXT (`host:track-devices`, `host:transport`, framing): https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/android15-tests-dev/SERVICES.TXT
- adb man page (port 5037): https://man.archlinux.org/man/adb.1.en.raw
- `ro.boot.qemu.avd_name` usage: https://github.com/dotnet/android-tools/commit/2bea0eb; `ro.boot.qemu.*` population: https://gerrit.omnirom.org/plugins/gitiles/android_system_core/+/bcc7946ef0883c73752251ee034b84c8416b48a5%5E%21
- Emulator `avd/running/pid_*.ini` discovery files: https://github.com/bridgerust/umide/pull/51; https://docs.rs/crate/android-emulator/0.2.0/source/examples/list_emulators.rs
- `dumpsys activity top` timeout on Android 14 / field changes: https://www.github.com/openatx/uiautomator2/issues/952
- Apple Silicon temperature via private IOHID/SMC: https://docs.rs/crate/macmon/0.8.0/source/src/sources.rs; https://docs.rs/crate/whirr/0.3.4/source/src/mac/hid_temp.rs; https://github.com/WHYBBE/SwiftTempBar; SMC key variance: https://kernel.org/doc/Documentation/devicetree/bindings/hwmon/apple%2Csmc-hwmon.yaml
- Apple thermal state: https://developer.apple.com/documentation/foundation/processinfo/thermalstate
- macOS quarantine and ad-hoc signing for CLIs: https://users.rust-lang.org/t/distributing-cli-apps-on-macos/70223; https://developer.apple.com/forums/thread/729336
- WebSocket proxy workarounds for tungstenite: https://github.com/snapview/tungstenite-rs/issues/177; https://chuxi.github.io/posts/websocket
- RUSTSEC-2025-0012 (backoff unmaintained → backon): https://rustsec.org/advisories/RUSTSEC-2025-0012
- secrets-patterns-db: https://github.com/mazen160/secrets-patterns-db; gitleaks rules: https://github.com/gitleaks/gitleaks/blob/master/config/gitleaks.toml
- GitHub artifact attestations: https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations
- Maestro debug output / test output dir: https://docs.maestro.dev/troubleshooting/debug-output; https://docs.maestro.dev/cli/test-output-directory
- Sibling research used: [`01-maestro-cli-internals.md`](./01-maestro-cli-internals.md) (launcher `exec`, dadb/adb server, shutdown hook, version and artifact layout).
