# 14: Load test of the Compose deployment (#141)

**Date:** 2026-11-09 · **Milestone:** M6 · **Tool:** [`fleet-loadgen`](../crates/fleet-loadgen) · **Assets:** [`14-assets/`](14-assets/) (raw runs, `run.sh`, `summarize.py`, the corpus) · **Reconnect jitter:** #138 is in this build, and the load generator's own copy was fixed too (`17c95a7`), because it had the same zero-jitter bug.

**Bottom line:** every scenario ran against the Compose stack at 100 agents (20 for the giant log), with 10 live viewers. In every run that completed, every event the agents sent was acknowledged: none was lost, and the server did not crash. The one run that did not complete is the skew scenario with a clock 8 days behind, which the server refuses by design (F3). Latency under a burst of 100 agents is queueing, not per-batch work: the server's ingest takes a fixed number of batches per second, and the agents wait behind the rest. The queue forms on rows that every batch of a run locks. The fix for two of those rows (flow definitions and condition fingerprints, `2213d7e`) removed their lock waits, but the after-fix latency could not be compared cleanly: the shared host was 2x busier during those runs. Under that pool pressure the server used to return a fatal batch error after 5 s of waiting for a connection; it now answers a retryable `overloaded` error that the agent waits out and resends (section 6, F1).

## 1. Setup

| | |
|---|---|
| Host | Apple M1 Pro, 10 cores, 16 GiB, macOS 26, **shared with other work** (see 2, and section 8) |
| Load during the runs | 1-minute load average 6 to 13 for the first matrix, 9 to 26 for the after-fix runs. `raw/<run>/host.txt` has the load before and after each run. A runaway shell of another session used about 80% of a core during the after-fix runs. |
| Server | Fleet built from this tree with `deploy/docker/Dockerfile` and run by `deploy/compose` under the project name `fleet-load141`. The first matrix ran on the tree at `7c3d9f5` (the loadgen commits do not change the server). The after-fix runs ran on the tree at `2213d7e`, which adds the store fix. |
| Stack | Caddy 2.10 (TLS, `wss://localhost`) → Fleet (one container, 256 KiB frames, default rate limits, 20 database connections, 5 s acquire timeout) → Postgres 18 → Garage v2.5.0 |
| Credentials | an API key for project `mobile-app` for the agents, a viewer token for the live sockets. Neither is in the repo or the results. |
| Client | `fleet-loadgen` release build on the same host, so its CPU and the server's share one machine |
| Database | `fleet` in Postgres, one database per stack; it grew across the runs (851 MB at the end, before cleanup) |

## 2. Method

- **Scale.** 100 agents, each with 4 devices and 3 flows of 2 steps: 11 700 events per run (the wide, corpus, unaligned and giant scenarios differ, see the table). The burst runs send each batch as soon as the previous one is acknowledged: 32 events per batch, no pause. All agents share one CI run ID, so they share one pipeline attempt. That is the worst case for the version counter (section 4).
- **Viewers.** 10 live sockets subscribe to the project like a dashboard. Latency is **ingest-to-live**: from an agent's send of a flow's live end to the viewer frame that shows the flow finished. Agents and viewers share one process, so both ends use one clock.
- **Measured.** The loadgen's own numbers (`report.json`: events, acked, latency percentiles), `docker stats` for the fleet, Postgres and proxy containers (`stats.csv`, one reading per container per loop, about 1 to 2 s apart on this host), the database size before and after (`pg.txt`), the server's log for the run (`server.log`), the host load (`host.txt`). The diagnosis runs also sampled `pg_stat_activity` once a second (`pg-activity.txt`) and the statements that lock waiters were stuck on (`lock.txt`). Those samplers were one-second `psql` loops around `run.sh`; their script is not committed, because it reads the credentials file.
- **Repeats.** Every scenario ran twice. A third run was made when the two repeats differed by more than 25% on events/s or on p95, measured as |a − b| / max(a, b). The table in section 3.3 shows which ones.
- **Runs that fail.** `run.sh` writes the loadgen's exit code. A run that the loadgen gives up on (the skew scenario) writes no `report.json`; its row shows the refusals from the server's log.
- **Pacing.** The pacing knob (`--pace-ms`, `--batch`) is the loadgen's own. `--batch 4 --pace-ms 1000` gives about 4 events/s per agent. No new knob was needed.
- The runs were not gated on the host's load: `run.sh` records the load and starts regardless.

## 3. Results

### 3.1 Scenarios

Each scenario is one `run.sh` call with these loadgen flags (all with 10 viewers unless noted). The `raw/` directory name is the run.

| scenario | flags | what it tests |
|---|---|---|
| baseline | `--agents 100` | the plain burst |
| storm | `--storm` | all connections dropped at half the events and reconnected together (reconnect storm, with #138's jitter) |
| duplicates | `--duplicates` | batches resent and shuffled (duplicate delivery) |
| reorder | `--reorder` | batches sent in reverse, no resends (out-of-order delivery) |
| gap | `--gap-events 40` | 40 events dropped under one `journal.gap` (a full agent buffer) |
| skew-plus5m | `--clock-skew-ms 300000` | the agent clock 5 minutes ahead |
| skew-minus8d | `--clock-skew-ms -691200000 --deadline-s 20` | the agent clock 8 days behind (the 7-day id rule) |
| wide | `--wide` | long names, 24 steps per flow, long failure messages |
| corpus | `--corpus corpus.txt` | hostile strings (script tags, escape sequences, template syntax) as names, steps and failures |
| unaligned | `--unaligned` | the final `commands.json` lines up with none of the log's steps |
| missing-step | `--missing-step` | each flow has one step the final file does not list |
| quiet | `--quiet-agents 5` | 5 agents go silent after their first events and hold their sockets |
| giant | `--agents 20 --failure-bytes 300000 --steps 200 --deadline-s 120` | 20 agents with a 300 KB failure message per flow and 200 steps (a giant log) |

### 3.2 Results of every run

Columns: events/s is the loadgen's events over its elapsed time. Latency is in milliseconds. CPU is per core (100% is one core). "server ERROR / WARN" counts the server's log lines of both kinds (the section 3.4 table splits them). Postgres growth is the change in `pg_database_size` during the run.

| run | agents | events | events/s | acked | viewers | latency p50 / p95 / p99 / max (ms) | fleet CPU mean / max (%) | fleet RAM max (MiB) | Postgres CPU mean / max (%) | Postgres RAM max (MiB) | Postgres growth (MiB) | loadgen exit | server ERROR / WARN |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline-after-r1 | 100 | 11700 | 491 | 11700 | 10 (12000 samples) | 3736.7 / 16998.5 / 17527.6 / 17840.3 | 76 / 132 | 35 | 655 / 1175 | 365 | 8.7 | 0 | 47 / 481 |
| baseline-after-r2 | 100 | 11700 | 910 | 11700 | 10 (12000 samples) | 4610.5 / 5619.7 / 5826.3 / 6522.7 | 77 / 122 | 49 | 538 / 731 | 391 | 8.7 | 0 | 0 / 476 |
| baseline-r1 | 100 | 11700 | 1962 | 11700 | 10 (12000 samples) | 924.2 / 3228.7 / 3562.9 / 3815.4 | 37 / 105 | 42 | 140 / 415 | 274 | 13.4 | 0 | 0 / 113 |
| baseline-r2 | 100 | 11700 | 1625 | 11700 | 10 (12000 samples) | 1362.1 / 3794.9 / 4062.3 / 4665.5 | 48 / 128 | 46 | 102 / 300 | 297 | 12.3 | 0 | 0 / 162 |
| corpus-r1 | 100 | 12900 | 1376 | 12900 | 10 (12000 samples) | 1608.5 / 4657.8 / 5301.5 / 7493.5 | 50 / 106 | 73 | 233 / 478 | 483 | 10.0 | 0 | 1 / 210 |
| corpus-r2 | 100 | 12900 | 1135 | 12900 | 10 (12000 samples) | 1666.1 / 4906.9 / 5630.2 / 6601.2 | 27 / 57 | 92 | 154 / 606 | 585 | 11.4 | 0 | 1 / 231 |
| diag-paced-100 | 100 | 11700 | 197 | 11700 | 10 (12000 samples) | 618.5 / 1990.4 / 2834.8 / 3134.5 | 55 / 100 | 114 | 489 / 828 | 602 | 14.4 | 0 | 0 / 330 |
| duplicates-r1 | 100 | 11700 | 1689 | 11700 | 10 (12000 samples) | 1447.2 / 2992.4 / 3170.1 / 3783.5 | 50 / 102 | 56 | 125 / 334 | 352 | 8.3 | 0 | 0 / 64 |
| duplicates-r2 | 100 | 11700 | 1249 | 11700 | 10 (12000 samples) | 1885.9 / 3911.7 / 4683.3 / 6398.8 | 30 / 93 | 90 | 200 / 452 | 558 | 8.2 | 0 | 0 / 164 |
| duplicates-r3 | 100 | 11700 | 1041 | 11700 | 10 (12000 samples) | 2917.2 / 4492.3 / 4989.0 / 6345.6 | 45 / 95 | 109 | 358 / 729 | 484 | 8.6 | 0 | 0 / 220 |
| gap-r1 | 100 | 11700 | 1060 | 11700 | 10 (7000 samples) | 3019.3 / 5502.3 / 5910.8 / 6526.4 | 33 / 89 | 63 | 187 / 527 | 386 | 7.6 | 0 | 0 / 348 |
| gap-r2 | 100 | 11700 | 1150 | 11700 | 10 (7000 samples) | 1563.3 / 4842.4 / 6208.3 / 7025.1 | 27 / 67 | 91 | 117 / 312 | 559 | 6.7 | 0 | 0 / 240 |
| giant-r1 | 20 | 144900 | 5203 | 144900 | 10 (2400 samples) | 156.5 / 329.7 / 454.6 / 841.8 | 96 / 140 | 89 | 415 / 595 | 561 | 109.4 | 0 | 0 / 1 |
| giant-r2 | 20 | 144900 | 2645 | 144900 | 10 (2400 samples) | 214.0 / 989.0 / 4822.4 / 5631.7 | 99 / 161 | 100 | 569 / 854 | 618 | 101.7 | 0 | 1 / 30 |
| giant-r3 | 20 | 144900 | 3144 | 144900 | 10 (2400 samples) | 214.1 / 1102.1 / 2237.0 / 2467.1 | 84 / 119 | 102 | 527 / 781 | 510 | 105.3 | 0 | 1 / 19 |
| lat-burst-10 | 10 | 1170 | 681 | 1170 | 10 (1200 samples) | 149.9 / 541.9 / 923.2 / 1084.9 | 12 / 21 | 108 | 88 / 138 | 481 | 0.8 | 0 | 0 / 0 |
| lat-burst-50 | 50 | 5850 | 1077 | 5850 | 10 (6000 samples) | 1233.0 / 1619.3 / 2670.5 / 4217.9 | 42 / 111 | 109 | 357 / 754 | 488 | 4.8 | 0 | 0 / 16 |
| lat-paced-1 | 1 | 117 | 3 | 117 | 10 (120 samples) | 110.3 / 394.6 / 394.9 / 395.4 | 3 / 8 | 117 | 54 / 124 | 493 | 0.4 | 0 | 0 / 0 |
| lat-paced-10 | 10 | 1170 | 34 | 1170 | 10 (1200 samples) | 189.6 / 329.5 / 344.6 / 345.4 | 11 / 42 | 113 | 129 / 249 | 581 | 1.6 | 0 | 0 / 0 |
| lat-paced-50 | 50 | 5850 | 158 | 5850 | 10 (6000 samples) | 78.7 / 934.0 / 1166.5 / 1346.6 | 24 / 35 | 109 | 246 / 511 | 480 | 7.5 | 0 | 0 / 10 |
| missing-step-r1 | 100 | 14100 | 1436 | 14100 | 10 (12000 samples) | 1359.8 / 4563.5 / 4924.0 / 4951.0 | 34 / 121 | 76 | 162 / 548 | 491 | 10.7 | 0 | 0 / 227 |
| missing-step-r2 | 100 | 14100 | 1446 | 14100 | 10 (12000 samples) | 1372.2 / 4670.4 / 5104.5 / 6312.3 | 32 / 89 | 93 | 153 / 430 | 545 | 9.4 | 0 | 0 / 202 |
| paced-100-after | 100 | 11700 | 219 | 11700 | 10 (12000 samples) | 584.3 / 3559.5 / 4260.5 / 4699.9 | 71 / 124 | 57 | 620 / 842 | 450 | 14.9 | 0 | 0 / 94 |
| paced-100-r1 | 100 | 11700 | 269 | 11700 | 10 (12000 samples) | 122.2 / 764.9 / 1154.2 / 1253.1 | 36 / 89 | 108 | 315 / 790 | 603 | 14.5 | 0 | 0 / 137 |
| quiet-r1 | 100 | 11700 | 1699 | 11700 | 10 (12000 samples) | 1274.6 / 3088.8 / 3506.2 / 4465.9 | 28 / 69 | 76 | 124 / 246 | 499 | 9.5 | 0 | 0 / 107 |
| quiet-r2 | 100 | 11700 | 1596 | 11700 | 10 (12000 samples) | 1284.1 / 3432.7 / 3856.0 / 4795.1 | 43 / 98 | 92 | 237 / 613 | 564 | 8.5 | 0 | 0 / 132 |
| reorder-r1 | 100 | 11700 | 1956 | 11700 | 10 (12000 samples) | 1141.4 / 2937.8 / 3183.8 / 3596.7 | 38 / 110 | 59 | 164 / 537 | 362 | 8.8 | 0 | 0 / 69 |
| reorder-r2 | 100 | 11700 | 932 | 11700 | 10 (11980 samples) | 1309.2 / 9347.4 / 10015.8 / 10383.2 | 45 / 101 | 91 | 249 / 643 | 569 | 9.1 | 0 | 38 / 214 |
| reorder-r3 | 100 | 11700 | 984 | 11700 | 10 (12000 samples) | 2842.9 / 6160.4 / 7046.3 / 8723.8 | 44 / 91 | 93 | 264 / 690 | 420 | 9.4 | 0 | 0 / 254 |
| skew-minus8d-r1 | - | - | - | - | - | no report (refused hellos: 698) | - | - | - | - | - | 1 | - |
| skew-minus8d-r2 | - | - | - | - | - | no report (refused hellos: 699) | - | - | - | - | - | 1 | - |
| skew-plus5m-r1 | 100 | 11700 | 1818 | 11700 | 10 (12000 samples) | 1005.7 / 3350.1 / 3667.0 / 3996.6 | 39 / 123 | 64 | 176 / 568 | 379 | 9.3 | 0 | 0 / 127 |
| skew-plus5m-r2 | 100 | 11700 | 1603 | 11700 | 10 (12000 samples) | 1411.0 / 3788.3 / 4094.3 / 5103.6 | 38 / 115 | 90 | 130 / 335 | 566 | 8.7 | 0 | 0 / 156 |
| storm-r1 | 100 | 11700 | 1332 | 11700 | 10 (12000 samples) | 1382.7 / 3116.8 / 3473.3 / 4563.3 | 50 / 102 | 54 | 282 / 575 | 334 | 9.1 | 0 | 0 / 101 |
| storm-r2 | 100 | 11700 | 1471 | 11700 | 10 (12000 samples) | 1316.6 / 2923.1 / 3285.0 / 3871.0 | 46 / 116 | 57 | 175 / 441 | 354 | 10.0 | 0 | 0 / 20 |
| unaligned-r1 | 100 | 9300 | 1376 | 9300 | 10 (12000 samples) | 1726.0 / 4169.7 / 4425.2 / 5033.2 | 27 / 82 | 74 | 167 / 537 | 495 | 7.4 | 0 | 0 / 176 |
| unaligned-r2 | 100 | 9300 | 1454 | 9300 | 10 (12000 samples) | 2183.6 / 4052.5 / 4342.9 / 4523.5 | 29 / 86 | 93 | 192 / 604 | 586 | 7.2 | 0 | 0 / 166 |
| wide-r1 | 100 | 90900 | 2881 | 90900 | 10 (12000 samples) | 1588.7 / 2585.3 / 4835.1 / 5135.6 | 75 / 139 | 76 | 431 / 754 | 476 | 87.5 | 0 | 0 / 219 |
| wide-r2 | 100 | 90900 | 2586 | 90900 | 10 (12000 samples) | 1665.1 / 3587.7 / 5270.1 / 5813.3 | 76 / 121 | 93 | 438 / 747 | 616 | 69.5 | 0 | 0 / 283 |
| wide-r3 | 100 | 90900 | 1662 | 90900 | 10 (12000 samples) | 2566.8 / 7815.6 / 12553.6 / 14824.3 | 59 / 116 | 94 | 500 / 783 | 469 | 67.5 | 0 | 19 / 506 |

The skew-minus8d rows have no loadgen report: the agents' hellos were refused, and the loadgen gave up after its 20 s deadline (exit 1). That is the expected result (section 6, F3).

### 3.3 Repeats and the third-run rule

| scenario | repeats | events/s (r1, r2, ...) | p95 ms (r1, r2, ...) | events/s spread | p95 spread | third run needed |
|---|---|---|---|---|---|---|
| baseline | 2 | 1962, 1625 | 3229, 3795 | 17% | 15% | no |
| baseline-after | 2 | 491, 910 | 16999, 5620 | 46% | 67% | yes |
| corpus | 2 | 1376, 1135 | 4658, 4907 | 18% | 5% | no |
| duplicates | 3 | 1689, 1249, 1041 | 2992, 3912, 4492 | 26% | 24% | yes |
| gap | 2 | 1060, 1150 | 5502, 4842 | 8% | 12% | no |
| giant | 3 | 5203, 2645, 3144 | 330, 989, 1102 | 49% | 67% | yes |
| missing-step | 2 | 1436, 1446 | 4563, 4670 | 1% | 2% | no |
| quiet | 2 | 1699, 1596 | 3089, 3433 | 6% | 10% | no |
| reorder | 3 | 1956, 932, 984 | 2938, 9347, 6160 | 52% | 69% | yes |
| skew-plus5m | 2 | 1818, 1603 | 3350, 3788 | 12% | 12% | no |
| storm | 2 | 1332, 1471 | 3117, 2923 | 9% | 6% | no |
| unaligned | 2 | 1376, 1454 | 4170, 4052 | 5% | 3% | no |
| wide | 3 | 2881, 2586, 1662 | 2585, 3588, 7816 | 10% | 28% | yes |

Third runs were made where the rule asked (duplicates, giant, reorder, wide). The third run is in the table above. The spread is large on this host: the same scenario moved by up to 67% at p95 between runs. The numbers in section 3.2 are the results, not averages.

### 3.4 Server log lines by kind

Counts of lines in each run's `server.log`. "pool acquire slow" and "slow statement" are sqlx warnings: the first says a connection took more than 2 s to come out of the pool, the second says a statement went over sqlx's slow-statement threshold. "batch error" is the server's `applying a batch … database query failed` line, which closes the agent's socket (F1). "hello refused" is a refused agent start (F3).

| run | pool acquire slow | slow statement | hello refused | batch error | other ERROR | other WARN |
|---|---|---|---|---|---|---|
| baseline-after-r1 | 392 | 39 | 0 | 46 | 1 | 50 |
| baseline-after-r2 | 464 | 12 | 0 | 0 | 0 | 0 |
| baseline-r1 | 91 | 22 | 0 | 0 | 0 | 0 |
| baseline-r2 | 138 | 24 | 0 | 0 | 0 | 0 |
| corpus-r1 | 173 | 37 | 0 | 1 | 0 | 0 |
| corpus-r2 | 178 | 52 | 0 | 1 | 0 | 1 |
| diag-paced-100 | 229 | 100 | 0 | 0 | 0 | 1 |
| duplicates-r1 | 48 | 16 | 0 | 0 | 0 | 0 |
| duplicates-r2 | 127 | 37 | 0 | 0 | 0 | 0 |
| duplicates-r3 | 186 | 34 | 0 | 0 | 0 | 0 |
| gap-r1 | 303 | 45 | 0 | 0 | 0 | 0 |
| gap-r2 | 192 | 47 | 0 | 0 | 0 | 1 |
| giant-r1 | 0 | 1 | 0 | 0 | 0 | 0 |
| giant-r2 | 1 | 28 | 0 | 1 | 0 | 1 |
| giant-r3 | 0 | 18 | 0 | 1 | 0 | 1 |
| lat-burst-10 | 0 | 0 | 0 | 0 | 0 | 0 |
| lat-burst-50 | 0 | 16 | 0 | 0 | 0 | 0 |
| lat-paced-1 | 0 | 0 | 0 | 0 | 0 | 0 |
| lat-paced-10 | 0 | 0 | 0 | 0 | 0 | 0 |
| lat-paced-50 | 0 | 10 | 0 | 0 | 0 | 0 |
| missing-step-r1 | 186 | 41 | 0 | 0 | 0 | 0 |
| missing-step-r2 | 175 | 27 | 0 | 0 | 0 | 0 |
| paced-100-after | 68 | 25 | 0 | 0 | 0 | 1 |
| paced-100-r1 | 77 | 60 | 0 | 0 | 0 | 0 |
| quiet-r1 | 82 | 25 | 0 | 0 | 0 | 0 |
| quiet-r2 | 108 | 24 | 0 | 0 | 0 | 0 |
| reorder-r1 | 49 | 20 | 0 | 0 | 0 | 0 |
| reorder-r2 | 120 | 52 | 0 | 38 | 0 | 42 |
| reorder-r3 | 206 | 47 | 0 | 0 | 0 | 1 |
| skew-minus8d-r1 | 0 | 0 | 698 | 0 | 0 | 0 |
| skew-minus8d-r2 | 0 | 0 | 699 | 0 | 0 | 0 |
| skew-plus5m-r1 | 102 | 25 | 0 | 0 | 0 | 0 |
| skew-plus5m-r2 | 134 | 21 | 0 | 0 | 0 | 1 |
| storm-r1 | 79 | 22 | 0 | 0 | 0 | 0 |
| storm-r2 | 0 | 20 | 0 | 0 | 0 | 0 |
| unaligned-r1 | 150 | 26 | 0 | 0 | 0 | 0 |
| unaligned-r2 | 143 | 23 | 0 | 0 | 0 | 0 |
| wide-r1 | 177 | 41 | 0 | 0 | 0 | 1 |
| wide-r2 | 245 | 38 | 0 | 0 | 0 | 0 |
| wide-r3 | 407 | 82 | 0 | 19 | 0 | 17 |

## 4. Latency diagnosis

### 4.1 Where the time goes at 100 agents in a burst

The burst runs are closed loops: each agent waits for its batch's acknowledgement before it sends the next. So the batch round trip R, the number of agents N and the throughput X (batches per second) satisfy N = X × R (Little's law). The baseline runs had about 1 960 events/s, which is about 61 batches/s of 32 events. With N = 100 that predicts R ≈ 1.6 s, and the observed batch round trip is the same order (the viewer p50 is 0.9 to 1.4 s, the pool acquire median 2.3 s). The server is not doing 1.6 s of work per batch. It is serving about 61 batches/s, and the agents queue behind each other. The latency is queueing at ingest.

At 10 agents in a burst the server kept up (1 170 events, p50 150 ms, p95 540 ms). At 50 agents the queue formed (p50 1.2 s, p95 1.6 s). The per-batch cost did not change; the queue did.

### 4.2 The floor at low rate

With one agent sending about 3 events/s the p50 is 110 ms and the p95 395 ms (twelve flows, 120 samples). With ten agents at about 3 events/s each, the p50 is 190 ms, p95 330 ms. With 50 agents at about 3 events/s each, p50 79 ms but p95 934 ms. So there is a floor of roughly 100 ms per flow end on an idle stack, and it is not from contention. The floor was not broken down further; the candidates are the batch commit, the publish's entity read (a query run after the commit, before the frame is ordered for the viewers), the ordered publish, the proxy's TLS and the viewer's own read. Only the first two touch Postgres.

### 4.3 What the 100-agent paced runs wait on

With 100 agents at about 2 to 3 events/s each (197 to 269 events/s in total), the p95 was 765 ms in one run and 1 990 ms in the sampled run. The sampled run is the one the lock analysis below uses. In that run, the Postgres container used 489% CPU on average (about five cores' worth of the shared host) against 55% for the fleet. Postgres is the heavier process. The pool of 20 connections was not free: 229 pool-acquire warnings in that run.

Sampling `pg_stat_activity` and the statements that lock waiters were stuck on (`lock.txt`) gives 251 waiting samples in the sampled run, and 9 samples of commits waiting on the write-ahead log (`IO/WalSync`):

| statement that waits | waiting samples before the fix | after the fix (paced 100 run) |
|---|---|---|
| `INSERT INTO flow_definitions … ON CONFLICT DO UPDATE` (every flow.reported batch) | 115 | 0 |
| `INSERT INTO condition_fingerprints … ON CONFLICT DO UPDATE` (every final flow) | 14 | 0 |
| `SELECT … FROM agent_runs WHERE id = $1 FOR UPDATE` (the batch and the telemetry commit of one run racing) | 62 | 13 |
| `UPDATE pipeline_attempts SET version = version + 1` (the version bump every committed batch makes) | 31 | 47 |
| `INSERT INTO pipeline_runs … ON CONFLICT` (each agent opening its run) | 20 | 18 |
| `UPDATE agent_runs SET telemetry_committed` and the `last_frame_at` update | 9 | 3 |

The first two rows are the cause of most of the convoy and were the easiest to remove: every batch that named a flow or a condition took a write lock on its shared row, even when nothing changed, and held it until its batch committed. The fix is in section 6 (F2). The version bump is the design, not a bug: versions are the order of the live deltas (research 03 §5.3), so the bump serialises the commits of one pipeline attempt by design. With every agent in one CI run, that is one lock for the whole run. It is the next place to look, along with the per-run `FOR UPDATE` and the open-run upsert, which only matter at start.

### 4.4 Verdict

The latency comes from the server's write path, not from the host alone and not from the client:

- **At 100 agents in a burst,** the batches queue on a small number of shared rows, mainly the flow and condition upserts and the version bump of the one pipeline attempt, and on the 20 pooled connections waiting for them. Throughput is fixed at about 60 batches/s on this host, so the latency scales with the number of agents (Little's law).
- **At low rate,** the floor of about 100 ms per flow end is not contention. It is the fixed cost of commit, publish and fan-out. It was not broken down further.
- **The shared host** makes all of it worse: Postgres and the fleet share the machine with other work, and the load rose from 6–13 to 9–26 between the two sets of runs. The lock waits are the server's own; the host adds CPU queueing on top of them.

## 5. What was not compared after the fix

The fix (F2) was verified in two ways: its tests, which fail when the upsert is put back (section 6), and the lock sampling above, which shows the two statements gone from the waiters. The end-to-end latency was measured after the fix only twice at 100 agents (`baseline-after-r1` and `-r2`) and once paced (`paced-100-after`). Those runs were on a host at load 9–26, against 6–13 for the before runs. Their numbers are in the table of section 3.2, and they are not evidence of a regression or of an improvement. The after-fix runs also hit the 5 s pool timeouts (F1). A clean before/after needs an idle host, and the one this was run on was not idle.

## 6. Failures and fixes

| | failure | seen in | cause | status |
|---|---|---|---|---|
| F1 | the server returns `database query failed` for a batch, logs ERROR, closes the agent's socket with a fatal error; the live delta load fails the same way and sends a resync | reorder-r2 (38 batch errors), wide-r3 (19), giant-r2 and -r3 (1 each), corpus-r1 and -r2 (1 each), baseline-after-r1 (46) | the pool's acquire timeout, 5 s: `acquired_after_secs` reaches 5.00 in those runs. The pool of 20 is full of transactions waiting on the rows in 4.3. | **fixed.** A pool acquire timeout is now `ApplyError::Busy` (`58326b2`, fleet-store), and the server answers a non-fatal `error{overloaded}` with the batch's first `seq` and keeps the socket (`16941d2`, fleet-server). Nothing of the batch was committed: its one transaction never started. The agent waits 200 ms to 5 s (jittered, reset by an ack) and resends from `expected_seq` on the same socket (`a2d5505`, fleet-agent). Loadgen does the same and counts the refusals as `busy` (`603508e`). Every other store error stays fatal. Tests: `ingest::tests::a_batch_without_a_connection_is_busy_and_writes_nothing` (fleet-store), `ingest::tests::a_pool_that_times_out_refuses_the_batch_and_keeps_the_socket`, `another_store_error_is_still_fatal` (fleet-server), `transport::tests::a_busy_batch_is_resent_on_the_same_socket_and_acked`, `busy_refusals_rewind_to_their_seq_and_an_ack_ends_the_streak`, `overload_waits_grow_from_200_ms_to_5_s_and_are_jittered` (fleet-agent), `busy_waits_grow_from_200_ms_to_5_s_and_stay_under_their_ceiling` (fleet-loadgen). The matrix above ran on the pre-fix build. The after-fix rerun (reorder and wide against a fresh stack) was not made: the host load was 22.5 (1 minute) and 29.8 (15 minutes) on 10 cores when checked, above the threshold of 10 for a clean comparison, so the ERROR counts above are not yet compared with the fix. |
| F2 | the batches of a run queue on the rows of flows and conditions they all share | every burst and paced run (section 4.3) | every batch took a write lock on each flow definition and condition fingerprint it named, even when nothing changed, and held it until its commit | **fixed** in `2213d7e`: a known flow name and a known condition are read first and written only when something changed. Tests: `ingest::apply::tests::a_known_flow_name_does_not_wait_for_the_row`, `ingest::analytics::tests::a_known_condition_does_not_wait_for_the_row`, `a_new_name_renames_the_flow`. Each holds the row from another transaction and fails with a timeout when the read is removed (mutation-checked). The lock waits on those two statements went to 0; the lock waits of the paced run went from 251 to 84 samples. |
| F3 | a clock 8 days behind is refused: `Invalid agent_run_id must be a current UUIDv7` | skew-minus8d-r1 (698 refusals), -r2 (699) | the 7-day id rule. An agent whose clock is 8 days behind stamps ids the server does not accept. | **expected, not a bug.** The loadgen gives up after its 20 s deadline (exit 1) and writes no report. Limit: the loadgen writes its report only on success. |

## 7. Limits

- **A shared, busy host.** Load 6 to 26 on 10 cores during the runs, with another session's runaway shell at about 80% of a core. Client, Docker VM, Postgres and the server all share it. The spread between repeats (up to 67% at p95) is the host, not the server. The after-fix runs are not comparable to the before runs for latency (section 5).
- **One machine.** The loadgen runs on the same host as the server, so its CPU competes with the server's. The client is not a fair load at 100 agents on this host.
- **One Fleet instance,** so the cross-instance notify relay (`pg_notify` between instances, research 03 §5.5) was not exercised.
- **No TLS offload.** The proxy terminates TLS locally with a private CA (`--ca-file`); the production proxy was not tested.
- **Scale.** 100 agents and 10 viewers. No run had 1 000 agents or 100 viewers.
- **Paced rate.** `--batch 4 --pace-ms 1000` is about 4 events/s per agent. It was not varied beyond the 1, 10, 50 and 100 agent runs above.
- **One CI run per scenario.** All agents of a scenario share one pipeline attempt. Real CI spreads agents over runs; the version bump (4.3) is the worst case for that reason.
- **The matrix ran on the pre-fix build.** Only `baseline` (twice) and `paced-100` ran on the post-fix build. The other scenarios' failures (F1) were not re-run after the fix, because the failure was not fixed by it.
- **The image rebuild is slow.** The source layer changes with any edit, so Docker runs the `trunk` build of the wasm dashboard again. The rebuild after the store fix took about 10 minutes of wall time. That is a build-process cost, not a runtime one.

## 8. Cleanup

After this document was committed: `docker compose -p fleet-load141 down -v` (from `deploy/compose`, same env), `docker image rm fleet-load141:local`, and the loadgen release build (`target/release`) was removed. The Garage and Postgres images are kept. No container, volume or image of `fleet-load141` remains (checked with `docker ps -a`, `docker volume ls`, `docker images`).

## 9. Reproduce

```sh
# from deploy/compose, with FLEET_IMAGE=fleet-load141:local
docker compose -p fleet-load141 up -d --build
# from the repo root, with FLEET_TOKEN, FLEET_VIEWER_TOKEN and FLEET_CA_FILE set
sh research/14-assets/run.sh baseline-r1 --agents 100 --viewers 10 --deadline-s 120 --run-id base-r1
python3 research/14-assets/summarize.py
```
