#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Throwaway benchmark for issue #161 (research/13-coordinate-mode.md). Not product code.

Modes, all on the same emulators and the same suite (suite/, 12 flows):

  shard-split  one `maestro test --shard-split N` over all devices (equal-count CI-style split)
  static       one `maestro test` process per shard of a fleet-planner manifest, one device each
  dynamic      the prototype: a queue of flows, each free device takes the next flow
  serial       one `maestro test` process per flow, one device, back to back (calibration and
               per-flow overhead: the N-invocation side)
  single       one `maestro test` process over the whole suite, one device (per-flow overhead:
               the one-invocation side)
  estimates    median per-flow flow time over several `serial` runs, as a planner estimates file
  summary      per-mode medians and spreads over result.json files

Every run writes <out>/result.json: pipeline wall time, busy and idle time per device, every
attempt of every flow (device, start, end, exit code, outcome, source), and the kills.

Outcomes come from two sources, as in Fleet's claim lattice: the console line
`[Passed] <flow> (<dur>)` / `[Failed] <flow> (<dur>)` (provisional, written as it happens) and the
JUnit report (final, written only at the end of a process, so a killed process leaves none).
"""

import argparse
import json
import os
import re
import signal
import statistics
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET
from collections import defaultdict, deque
from pathlib import Path

HERE = Path(__file__).resolve().parent
SUITE = HERE / "suite"
CONFIG = SUITE / "config.yaml"
MAESTRO = os.environ.get("MAESTRO", "/opt/homebrew/bin/maestro")
ENV = dict(os.environ, MAESTRO_CLI_NO_ANALYTICS="1", MAESTRO_DISABLE_UPDATE_CHECK="true")
MAX_ATTEMPTS = 2  # infrastructure re-runs only (a killed process), never after a test verdict
PROCESS_TIMEOUT = 900  # seconds; a watchdog, not a tuned value
PASSED = re.compile(r"\[Passed\] (\S+) \(([^)]*)\)")
FAILED = re.compile(r"\[Failed\] (\S+) \(([^)]*)\)")
SELECTED = re.compile(r"\[shard (\d+)\] Selected device (\S+) using port")


def flow_files():
    return sorted(SUITE.glob("f*.yaml"))


def flow_name(path):
    return path.stem


def repo_rel(path):
    return str(path.relative_to(HERE.parent.parent))


def parse_duration(text):
    """Maestro prints `18s`, `1m 3s`; returns seconds as a float."""
    total = 0.0
    for value, unit in re.findall(r"(\d+)\s*(ms|h|m|s)", text):
        total += int(value) * {"ms": 0.001, "s": 1, "m": 60, "h": 3600}[unit]
    return total


def scan_log(log_path):
    """Console outcomes: [(flow, passed, seconds)] in the order they were printed."""
    out = []
    if not log_path.exists():
        return out
    for line in log_path.read_text(errors="replace").splitlines():
        m = PASSED.search(line)
        if m:
            out.append((m.group(1), True, parse_duration(m.group(2))))
            continue
        m = FAILED.search(line)
        if m:
            out.append((m.group(1), False, parse_duration(m.group(2))))
    return out


def scan_junit(path):
    """Final outcomes: [(flow, passed, seconds, suite_index)]. Empty if the report is missing."""
    out = []
    if not path.exists() or path.stat().st_size == 0:
        return out
    try:
        root = ET.parse(path).getroot()
    except ET.ParseError:
        return out
    for index, suite in enumerate(root.iter("testsuite")):
        for case in suite.iter("testcase"):
            failed = case.find("failure") is not None or case.find("error") is not None
            passed = not failed and case.get("status", "SUCCESS") == "SUCCESS"
            out.append((case.get("name"), passed, float(case.get("time") or 0), index + 1))
    return out


class Slot:
    """What a worker is running right now; the killer reads it."""

    def __init__(self, serial):
        self.serial = serial
        self.proc = None
        self.item = None
        self.occupied = 0.0  # seconds a maestro process was running on this device


def run_process(argv, log_path, slot, item):
    log_path.parent.mkdir(parents=True, exist_ok=True)
    started = time.time()
    with open(log_path, "wb") as log:
        proc = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT, env=ENV,
                                start_new_session=True)
    slot.proc, slot.item = proc, item
    try:
        rc = proc.wait(timeout=PROCESS_TIMEOUT)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        rc = proc.wait()
    ended = time.time()
    slot.proc, slot.item = None, None
    slot.occupied += ended - started
    return {"rc": rc, "start": started, "end": ended}


def maestro_argv(serial, junit, debug_dir, targets):
    return [MAESTRO, "--device", serial, "test", "--config", str(CONFIG), "--format", "junit",
            "--output", str(junit), "--debug-output", str(debug_dir)] + [str(t) for t in targets]


def arm_killer(kill, slots, t0, out):
    """SIGKILLs the maestro process of one device `seconds` into the run, once."""
    if not kill:
        return
    serial, seconds = kill

    def fire():
        time.sleep(max(0.0, t0 + seconds - time.time()))
        proc, item = slots[serial].proc, slots[serial].item
        if proc is None:
            out["kill"] = {"serial": serial, "at": round(time.time() - t0, 3), "hit": False}
            return
        out["kill"] = {"serial": serial, "at": round(time.time() - t0, 3), "hit": True,
                       "running": item}
        os.killpg(proc.pid, signal.SIGKILL)

    threading.Thread(target=fire, daemon=True).start()


def prepare_devices(devices):
    for serial in devices:
        subprocess.run(["adb", "-s", serial, "shell", "am", "force-stop", "com.fleet.anr"],
                       check=False, capture_output=True)
    time.sleep(3)


def session_shard_map(debug_dir):
    """`[shard N] Selected device <serial>` lines from Maestro's session log(s)."""
    mapping = {}
    for log in debug_dir.rglob("maestro.log"):
        for m in SELECTED.finditer(log.read_text(errors="replace")):
            mapping[int(m.group(1))] = m.group(2)
    return mapping


def process_outcomes(rundir, tag, serial, debug_dir, shard_split=False):
    """Outcomes one process produced: one row per flow, with the device it ran on.

    JUnit is final and written at the end of the process; the console lines are the provisional
    view. A killed process leaves no JUnit, so its console lines are all there is. A row with
    `duplicate_reports` means the same flow was reported more than once by this process.
    """
    console = scan_log(rundir / "logs" / f"{tag}.log")
    final = scan_junit(rundir / "junit" / f"{tag}.xml")
    shard_map = session_shard_map(debug_dir) if shard_split else {}
    rows = []
    if final:
        counts = defaultdict(int)
        for name, passed, secs, suite in final:
            counts[name] += 1
            device = shard_map.get(suite, serial)
            rows.append({"flow": name, "device": device,
                         "outcome": "passed" if passed else "failed", "source": "junit",
                         "seconds": secs, "suite": suite})
    else:
        counts = defaultdict(int)
        for name, passed, secs in console:
            counts[name] += 1
            rows.append({"flow": name, "device": serial,
                         "outcome": "passed" if passed else "failed", "source": "console",
                         "seconds": secs, "suite": None})
    for name, n in counts.items():
        if n > 1:
            rows.append({"flow": name, "duplicate_reports": n})
    return rows


def record(attempts, tag, rows, run, device, expected=()):
    """Adds one process's outcomes to the per-flow attempt list.

    `expected` lists the flows the process was given. One of them without a verdict is recorded
    as interrupted: the process was killed or cut off before it reported that flow.
    """
    seen = set()
    for row in rows:
        flow = row["flow"]
        if "duplicate_reports" in row:
            attempts[flow].append({"process": tag, "duplicate_reports": row["duplicate_reports"]})
            continue
        seen.add(flow)
        attempts[flow].append({
            "process": tag, "device": row["device"], "start": run["start"], "end": run["end"],
            "rc": run["rc"], "outcome": row["outcome"], "source": row["source"],
            "seconds": row["seconds"], "suite": row["suite"],
        })
    for flow in expected:
        if flow not in seen:
            attempts[flow].append({
                "process": tag, "device": device, "start": run["start"], "end": run["end"],
                "rc": run["rc"], "outcome": "interrupted", "source": "none", "seconds": None,
                "suite": None,
            })


def expected_outcomes(flows):
    return {flow_name(f): ("failed" if flow_name(f) == "f12" else "passed") for f in flows}


def finish(out, rundir, mode, devices, t0, t1, slots, attempts, expected):
    lost = [f for f in expected if not any(a.get("outcome") in ("passed", "failed")
                                           for a in attempts.get(f, []))]
    correctness = {}
    for flow, want in expected.items():
        recs = attempts.get(flow, [])
        completed = [a for a in recs if a.get("outcome") in ("passed", "failed")]
        dupes = sum(a.get("duplicate_reports", 0) for a in recs)
        final = completed[-1]["outcome"] if completed else "missing"
        correctness[flow] = {
            "attempts": len(recs),
            "completed_attempts": len(completed),
            "final": final,
            "expected": want,
            "ran_exactly_once": len(completed) == 1 and dupes == 0,
            "matches": final == want,
        }
    flow_seconds = defaultdict(float)
    for flow, recs in attempts.items():
        for a in recs:
            if a.get("outcome") in ("passed", "failed") and a.get("seconds"):
                flow_seconds[a["device"]] += a["seconds"]
    occupied = {s: round((t1 - t0) if mode == "shard-split" else slots[s].occupied, 3)
                for s in devices}
    result = {
        "mode": mode,
        "devices": devices,
        "loadavg_1min_at_end": round(os.getloadavg()[0], 2),
        "pipeline_s": round(t1 - t0, 3),
        "device_occupied_s": occupied,
        "device_idle_s": {s: round((t1 - t0) - occupied[s], 3) for s in devices},
        "device_flow_s": {s: round(flow_seconds.get(s, 0.0), 3) for s in devices},
        "device_flow_idle_s": {s: round((t1 - t0) - flow_seconds.get(s, 0.0), 3) for s in devices},
        "lost_flows": lost,
        "kill": out.get("kill"),
        "notes": out.get("notes", []),
        "flows": {k: v for k, v in attempts.items()},
        "correctness": correctness,
        "all_correct": not lost and all(c["matches"] and c["ran_exactly_once"]
                                        for c in correctness.values()),
    }
    (rundir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: result[k] for k in ("mode", "pipeline_s", "device_idle_s",
                                              "lost_flows", "kill", "all_correct")}))
    return result


def run_shard_split(args, rundir, devices, flows):
    attempts = defaultdict(list)
    slots = {s: Slot(s) for s in devices}
    prepare_devices(devices)
    tag = "shard-split"
    argv = [MAESTRO, "--device", ",".join(devices), "test", "--shard-split", str(len(devices)),
            "--config", str(CONFIG), "--format", "junit",
            "--output", str(rundir / "junit" / f"{tag}.xml"),
            "--debug-output", str(rundir / "debug" / tag), str(SUITE)]
    t0 = time.time()
    run = run_process(argv, rundir / "logs" / f"{tag}.log", slots[devices[0]], tag)
    t1 = time.time()
    rows = process_outcomes(rundir, tag, devices[0], rundir / "debug" / tag, shard_split=True)
    record(attempts, tag, rows, run, devices[0])
    return finish({"notes": ["one process over all devices: no kill applied"]}, rundir,
                  "shard-split", devices, t0, t1, slots, attempts, expected_outcomes(flows))


def run_static(args, rundir, devices, flows):
    shards = json.loads(Path(args.plan).read_text())["shards"]
    if len(shards) != len(devices):
        sys.exit(f"plan has {len(shards)} shards for {len(devices)} devices")
    attempts = defaultdict(list)
    slots = {s: Slot(s) for s in devices}
    prepare_devices(devices)
    out = {"notes": []}
    results = {}

    def worker(shard, serial):
        tag = f"shard-{shard['id']}"
        targets = [HERE.parent.parent / f for f in shard["flows"]]
        run = run_process(maestro_argv(serial, rundir / "junit" / f"{tag}.xml",
                                       rundir / "debug" / tag, targets),
                          rundir / "logs" / f"{tag}.log", slots[serial], tag)
        results[tag] = (serial, run)

    t0 = time.time()
    arm_killer(args.kill, slots, t0, out)
    threads = [threading.Thread(target=worker, args=(sh, dev)) for sh, dev in zip(shards, devices)]
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    t1 = time.time()
    for tag, (serial, run) in results.items():
        rows = process_outcomes(rundir, tag, serial, rundir / "debug" / tag)
        record(attempts, tag, rows, run, serial)
    if args.kill:
        out["notes"].append("a killed static shard is not re-run: its unfinished flows are lost")
    return finish(out, rundir, "static", devices, t0, t1, slots, attempts, expected_outcomes(flows))


def run_dynamic(args, rundir, devices, flows):
    """The prototype: a queue of batches of flows; each free device takes the next batch.

    Batches are consecutive runs of the longest-first order, so a batch holds flows of similar
    length. A batch with no verdict for some flow (its process was killed) goes back to the head
    of the queue for those flows, at most MAX_ATTEMPTS times in all. A test verdict is never
    re-run.
    """
    est = json.loads(Path(args.estimates).read_text()) if args.estimates else {}
    by_name = {Path(k).stem: v for k, v in est.items()}
    ordered = sorted(flows, key=lambda f: -by_name.get(flow_name(f), 0))  # longest first (LPT)
    size = max(1, args.batch)
    queue = deque(ordered[i:i + size] for i in range(0, len(ordered), size))
    lock = threading.Lock()
    attempts = defaultdict(list)
    attempt_no = defaultdict(int)
    slots = {s: Slot(s) for s in devices}
    prepare_devices(devices)
    out = {"notes": []}
    t0 = time.time()
    arm_killer(args.kill, slots, t0, out)
    mode = "dynamic" if size == 1 else f"dynamic-batch{size}"
    batch_no = [0]

    def worker(serial):
        while True:
            with lock:
                if not queue:
                    return
                batch = queue.popleft()
                batch_no[0] += 1
                number = batch_no[0]
                for flow in batch:
                    attempt_no[flow] += 1
            names = [flow_name(f) for f in batch]
            tag = f"b{number:03d}.{serial}"
            run = run_process(maestro_argv(serial, rundir / "junit" / f"{tag}.xml",
                                           rundir / "debug" / tag, batch),
                              rundir / "logs" / f"{tag}.log", slots[serial], "+".join(names))
            rows = process_outcomes(rundir, tag, serial, rundir / "debug" / tag)
            record(attempts, tag, rows, run, serial, expected=names)
            verdicts = {r["flow"] for r in rows if "outcome" in r}
            retry = [f for f in batch if flow_name(f) not in verdicts
                     and attempt_no[f] < MAX_ATTEMPTS]
            if retry:
                with lock:
                    queue.appendleft(retry)
                out["notes"].append(f"{[flow_name(f) for f in retry]} re-queued: no verdict "
                                    f"from {tag}")

    threads = [threading.Thread(target=worker, args=(s,)) for s in devices]
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    t1 = time.time()
    return finish(out, rundir, mode, devices, t0, t1, slots, attempts, expected_outcomes(flows))


def run_serial(args, rundir, serial, flows):
    attempts = defaultdict(list)
    slots = {serial: Slot(serial)}
    prepare_devices([serial])
    out = {"notes": []}
    t0 = time.time()
    for flow in flows:
        name = flow_name(flow)
        tag = f"{name}.serial"
        run = run_process(maestro_argv(serial, rundir / "junit" / f"{tag}.xml",
                                       rundir / "debug" / tag, [flow]),
                          rundir / "logs" / f"{tag}.log", slots[serial], name)
        rows = process_outcomes(rundir, tag, serial, rundir / "debug" / tag)
        record(attempts, tag, rows, run, serial, expected=[name])
    t1 = time.time()
    return finish(out, rundir, "serial", [serial], t0, t1, slots, attempts, expected_outcomes(flows))


def run_single(args, rundir, serial, flows):
    attempts = defaultdict(list)
    slots = {serial: Slot(serial)}
    prepare_devices([serial])
    out = {"notes": []}
    t0 = time.time()
    tag = "single"
    run = run_process(maestro_argv(serial, rundir / "junit" / f"{tag}.xml",
                                   rundir / "debug" / tag, [SUITE]),
                      rundir / "logs" / f"{tag}.log", slots[serial], tag)
    t1 = time.time()
    rows = process_outcomes(rundir, tag, serial, rundir / "debug" / tag)
    record(attempts, tag, rows, run, serial)
    return finish(out, rundir, "single", [serial], t0, t1, slots, attempts, expected_outcomes(flows))


def cmd_estimates(args):
    samples = defaultdict(list)
    for run_dir in args.runs:
        data = json.loads((Path(run_dir) / "result.json").read_text())
        for flow, attempts in data["flows"].items():
            for a in attempts:
                if a["outcome"] in ("passed", "failed") and a["seconds"] is not None:
                    samples[flow].append(a["seconds"])
    estimates = {}
    for flow in sorted(samples):
        # The planner's unit is whole seconds (a u64 in its estimates file).
        estimates[repo_rel(SUITE / f"{flow}.yaml")] = max(1, round(statistics.median(samples[flow])))
    Path(args.out).write_text(json.dumps(estimates, indent=2) + "\n")
    print(json.dumps(estimates, indent=2))


def cmd_summary(args):
    rows = defaultdict(list)
    for run_dir in args.runs:
        data = json.loads((Path(run_dir) / "result.json").read_text())
        rows[data["mode"]].append(data)
    for mode, runs in rows.items():
        times = [r["pipeline_s"] for r in runs]
        idle = [sum(r["device_idle_s"].values()) for r in runs]
        print(f"{mode:12s} n={len(times)} pipeline_s={sorted(times)} "
              f"p50={statistics.median(times):.1f} spread={max(times) - min(times):.1f} "
              f"idle_total_s p50={statistics.median(idle):.1f} "
              f"all_correct={all(r['all_correct'] for r in runs)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("shard-split", "static", "dynamic", "serial", "single"):
        p = sub.add_parser(name)
        p.add_argument("--devices", required=True, help="comma-separated adb serials")
        p.add_argument("--out", required=True, help="run directory (created)")
        p.add_argument("--kill", nargs=2, metavar=("SERIAL", "SECONDS"),
                       help="SIGKILL that device's maestro process this many seconds in")
        if name == "static":
            p.add_argument("--plan", required=True, help="fleet-planner manifest")
        if name == "dynamic":
            p.add_argument("--estimates", help="planner estimates JSON, for longest-first order")
            p.add_argument("--batch", type=int, default=1,
                           help="flows per maestro invocation (1 = one process per flow)")
    p = sub.add_parser("estimates")
    p.add_argument("--runs", nargs="+", required=True)
    p.add_argument("--out", required=True)
    p = sub.add_parser("summary")
    p.add_argument("runs", nargs="+")
    args = parser.parse_args()

    if args.cmd == "estimates":
        return cmd_estimates(args)
    if args.cmd == "summary":
        return cmd_summary(args)
    rundir = Path(args.out).resolve()
    (rundir / "logs").mkdir(parents=True, exist_ok=True)
    (rundir / "junit").mkdir(parents=True, exist_ok=True)
    devices = args.devices.split(",")
    flows = flow_files()
    if args.kill:
        args.kill = (args.kill[0], float(args.kill[1]))
    if args.cmd == "shard-split":
        args.kill = None
        run_shard_split(args, rundir, devices, flows)
    elif args.cmd == "static":
        run_static(args, rundir, devices, flows)
    elif args.cmd == "dynamic":
        run_dynamic(args, rundir, devices, flows)
    elif args.cmd == "serial":
        run_serial(args, rundir, devices[0], flows)
    elif args.cmd == "single":
        run_single(args, rundir, devices[0], flows)


if __name__ == "__main__":
    main()
