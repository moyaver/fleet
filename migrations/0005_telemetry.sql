-- SPDX-License-Identifier: AGPL-3.0-only
-- 0005 telemetry (research 05 §10). Wide rows, NULL = unavailable (never zero), partitioned by
-- day on the sample time. The primary key dedups replays. Rollup tables are filled by the rollup
-- job; raw samples are kept 7 days by default.

CREATE TABLE host_samples (
  ts                   timestamptz NOT NULL,
  agent_run_id         uuid NOT NULL,
  worker_id            uuid,
  project_id           uuid NOT NULL,
  tseq                 bigint NOT NULL,
  host_cpu_pct         real,
  host_mem_used_bytes  bigint,
  host_mem_total_bytes bigint,
  maestro_cpu_pct      real,
  maestro_rss_bytes    bigint,
  agent_cpu_pct        real,
  agent_rss_bytes      bigint,
  PRIMARY KEY (agent_run_id, ts)
) PARTITION BY RANGE (ts);

CREATE TABLE device_samples (
  ts                        timestamptz NOT NULL,
  device_session_id         uuid NOT NULL,
  agent_run_id              uuid NOT NULL,
  project_id                uuid NOT NULL,
  emu_cpu_pct               real,
  emu_rss_bytes             bigint,
  adb_state                 smallint,
  boot_completed            boolean,
  guest_cpu_pct             real,
  guest_mem_available_bytes bigint,
  PRIMARY KEY (device_session_id, ts)
) PARTITION BY RANGE (ts);

-- Aggregates: n, and min/avg/max per metric.
CREATE TABLE host_rollup_1m (
  bucket            timestamptz NOT NULL,
  agent_run_id      uuid NOT NULL,
  worker_id         uuid,
  project_id        uuid NOT NULL,
  n                 integer NOT NULL,
  host_cpu_pct_avg  real, host_cpu_pct_max real,
  host_mem_used_avg bigint, host_mem_used_max bigint,
  maestro_cpu_avg   real, maestro_rss_max bigint,
  PRIMARY KEY (agent_run_id, bucket)
);
CREATE INDEX host_rollup_1m_worker ON host_rollup_1m (worker_id, bucket);

CREATE TABLE host_rollup_1h (LIKE host_rollup_1m INCLUDING ALL);

CREATE TABLE device_rollup_1m (
  bucket            timestamptz NOT NULL,
  device_session_id uuid NOT NULL,
  agent_run_id      uuid NOT NULL,
  project_id        uuid NOT NULL,
  n                 integer NOT NULL,
  emu_cpu_pct_avg   real, emu_cpu_pct_max real,
  emu_rss_max       bigint,
  PRIMARY KEY (device_session_id, bucket)
);

CREATE TABLE device_rollup_1h (LIKE device_rollup_1m INCLUDING ALL);

SELECT fleet_ensure_partitions('host_samples', (now() AT TIME ZONE 'UTC')::date - 1, (now() AT TIME ZONE 'UTC')::date + 7);
SELECT fleet_ensure_partitions('device_samples', (now() AT TIME ZONE 'UTC')::date - 1, (now() AT TIME ZONE 'UTC')::date + 7);
