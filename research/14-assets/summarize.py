#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Turns the raw runs under research/14-assets/raw/ into the tables of research/14-load-test.md.

    python3 research/14-assets/summarize.py [raw-dir]

Each run directory holds report.json (fleet-loadgen --report), stats.csv (docker stats, one reading
per container each loop), pg.txt, host.txt and server.log (see run.sh). A run the loadgen failed
has no report.json: it is listed with its exit code and the refusals in its log.

Prints three markdown tables: one row per run (sorted by name), the server's log lines by kind,
and the repeat check of research/14-load-test.md section 2 (a third run when two repeats differ by
more than 25% on events/s or p95, measured as |a - b| / max(a, b)).
"""
import csv
import json
import pathlib
import re
import sys

UNITS = {"B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3, "kB": 1000, "MB": 1000**2, "GB": 1000**3}
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# The log lines that matter, by kind. A line matches the first pattern that fits.
KINDS = [
    ("pool acquire slow", re.compile(r"pool::acquire.*exceeded slow threshold")),
    ("slow statement", re.compile(r"slow statement")),
    ("hello refused", re.compile(r"hello refused")),
    ("batch error", re.compile(r"ERROR .*applying a batch")),
    ("other ERROR", re.compile(r"\bERROR\b")),
    ("other WARN", re.compile(r"\bWARN\b")),
]


def mem_bytes(text):
    m = re.match(r"\s*([\d.]+)\s*([A-Za-z]+)", text)
    return float(m.group(1)) * UNITS.get(m.group(2), 1) if m else 0.0


def stats(path, container_suffix):
    """Mean and peak CPU (per core: 100% is one core) and the peak memory in MiB of one container."""
    cpu, mem = [], []
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.reader(f):
            if len(row) < 4 or not row[1].endswith(container_suffix):
                continue
            cpu.append(float(row[2].rstrip("%") or 0))
            mem.append(mem_bytes(row[3]))
    if not cpu:
        return None
    return sum(cpu) / len(cpu), max(cpu), max(mem) / 1024**2


def kv(path):
    out = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k] = v
    return out


def log_kinds(path):
    """Counts of the server's log lines by kind; a line is counted once, under its first kind."""
    counts = {name: 0 for name, _ in KINDS}
    if not path.exists():
        return counts
    for line in ANSI.sub("", path.read_text(encoding="utf-8", errors="replace")).splitlines():
        for name, pattern in KINDS:
            if pattern.search(line):
                counts[name] += 1
                break
    return counts


def relative(a, b):
    return abs(a - b) / max(a, b) if max(a, b) > 0 else 0.0


def main():
    raw = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else pathlib.Path(__file__).parent / "raw")
    runs = sorted(p for p in raw.iterdir() if p.is_dir())
    reports = {}

    print("| run | agents | events | events/s | acked | viewers | latency p50 / p95 / p99 / max (ms) | fleet CPU mean / max (%) | fleet RAM max (MiB) | Postgres CPU mean / max (%) | Postgres RAM max (MiB) | Postgres growth (MiB) | loadgen exit | server ERROR / WARN |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for run in runs:
        host = kv(run / "host.txt") if (run / "host.txt").exists() else {}
        exit_code = host.get("loadgen_exit", "n/a")
        report_file = run / "report.json"
        if not report_file.exists():
            refused = log_kinds(run / "server.log")["hello refused"]
            print(f"| {run.name} | - | - | - | - | - | no report (refused hellos: {refused}) | - | - | - | - | - | {exit_code} | - |")
            continue
        r = json.loads(report_file.read_text(encoding="utf-8"))
        reports[run.name] = r
        sc = r["scenario"]
        v = r.get("viewers")
        lat = v["latency_ms"] if v and v.get("latency_ms") else None
        latency = (
            f"{lat['p50']:.1f} / {lat['p95']:.1f} / {lat['p99']:.1f} / {lat['max']:.1f}" if lat else "n/a"
        )
        viewers = f"{v['viewers']} ({v['samples']} samples)" if v else "0"
        fleet = stats(run / "stats.csv", "-fleet-1") if (run / "stats.csv").exists() else None
        pgs = stats(run / "stats.csv", "-postgres-1") if (run / "stats.csv").exists() else None
        pg = kv(run / "pg.txt") if (run / "pg.txt").exists() else {}
        growth = "n/a"
        if pg:
            growth = f"{(int(pg['pg_database_size_after']) - int(pg['pg_database_size_before'])) / 1024**2:.1f}"
        kinds = log_kinds(run / "server.log")
        errs = kinds["batch error"] + kinds["other ERROR"]
        warns = sum(kinds[k] for k in ("pool acquire slow", "slow statement", "hello refused", "other WARN"))
        cpu = f"{fleet[0]:.0f} / {fleet[1]:.0f}" if fleet else "n/a"
        ram = f"{fleet[2]:.0f}" if fleet else "n/a"
        pg_cpu = f"{pgs[0]:.0f} / {pgs[1]:.0f}" if pgs else "n/a"
        pg_ram = f"{pgs[2]:.0f}" if pgs else "n/a"
        print(
            f"| {run.name} | {sc['agents']} | {r['events']} | {r['events_per_s']:.0f} | {r['acked']} | "
            f"{viewers} | {latency} | {cpu} | {ram} | {pg_cpu} | {pg_ram} | {growth} | {exit_code} | {errs} / {warns} |"
        )

    print()
    print("| run | pool acquire slow | slow statement | hello refused | batch error | other ERROR | other WARN |")
    print("|---|---|---|---|---|---|---|")
    for run in runs:
        if not (run / "server.log").exists():
            continue
        k = log_kinds(run / "server.log")
        print(
            f"| {run.name} | {k['pool acquire slow']} | {k['slow statement']} | {k['hello refused']} | "
            f"{k['batch error']} | {k['other ERROR']} | {k['other WARN']} |"
        )

    print()
    print("| scenario | repeats | events/s (r1, r2, ...) | p95 ms (r1, r2, ...) | events/s spread | p95 spread | third run needed |")
    print("|---|---|---|---|---|---|---|")
    scenarios = sorted({re.sub(r"-r\d+$", "", name) for name in reports})
    for sc_name in scenarios:
        names = sorted(n for n in reports if re.fullmatch(re.escape(sc_name) + r"-r\d+", n))
        rates = [reports[n]["events_per_s"] for n in names]
        p95s = [
            (reports[n].get("viewers") or {}).get("latency_ms", {}).get("p95") for n in names
        ]
        p95s = [p for p in p95s if p is not None]
        if len(names) < 2 or len(p95s) != len(names):
            continue
        rate_spread = relative(rates[0], rates[1])
        p95_spread = relative(p95s[0], p95s[1])
        needed = "yes" if rate_spread > 0.25 or p95_spread > 0.25 else "no"
        print(
            f"| {sc_name} | {len(names)} | {', '.join(f'{x:.0f}' for x in rates)} | "
            f"{', '.join(f'{x:.0f}' for x in p95s)} | {rate_spread:.0%} | {p95_spread:.0%} | {needed} |"
        )


if __name__ == "__main__":
    main()
