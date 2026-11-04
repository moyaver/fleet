-- SPDX-License-Identifier: AGPL-3.0-only
-- 0004 raw journal, gaps and rejections (research 05 §3.4, §9.2, §9.4; invariant I5).

-- The raw journal: every committed event, as sent, in the same transaction as its effects.
-- No FKs in or out, so partitions stay cheap to insert into and drop.
CREATE TABLE agent_events (
  run_day           date NOT NULL,
  agent_run_id      uuid NOT NULL,
  seq               bigint NOT NULL CHECK (seq >= 1),
  type              text NOT NULL,
  v                 smallint NOT NULL,
  src_kind          text NOT NULL,
  src_parser        text,
  occurred_at       timestamptz NOT NULL,
  mono_ms           bigint NOT NULL,
  received_at       timestamptz NOT NULL DEFAULT now(),
  subject           jsonb NOT NULL DEFAULT '{}',
  flow_execution_id uuid,
  -- applied: projected · unsupported: stored, not projected by this build · rejected: refused
  apply_status      text NOT NULL CHECK (apply_status IN ('applied', 'unsupported', 'rejected')),
  data              jsonb NOT NULL,
  PRIMARY KEY (run_day, agent_run_id, seq)
) PARTITION BY RANGE (run_day);

-- Explicit loss (I5): agent-declared `journal.gap`s, prefixes the agent no longer held at resume,
-- and telemetry the agent dropped.
CREATE TABLE ingest_gaps (
  agent_run_id uuid NOT NULL REFERENCES agent_runs,
  stream       text NOT NULL CHECK (stream IN ('journal', 'telemetry')),
  from_seq     bigint NOT NULL,
  to_seq       bigint NOT NULL,
  reason       text NOT NULL,
  recorded_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (agent_run_id, stream, from_seq),
  CHECK (from_seq <= to_seq)
);

-- Events refused for good; the cursor moved past them (`event_rejected`).
CREATE TABLE ingest_rejections (
  agent_run_id uuid NOT NULL REFERENCES agent_runs,
  seq          bigint NOT NULL,
  code         text NOT NULL,
  message      text,
  raw          jsonb,
  rejected_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (agent_run_id, seq)
);

-- Application-managed daily partitions (research 05 §9.1): idempotent, so start-up and the daily
-- sweep both call it. Names are <table>_pYYYYMMDD. No DEFAULT partition (§10.2).
CREATE FUNCTION fleet_ensure_partitions(parent regclass, from_day date, to_day date)
RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  d       date := from_day;
  created integer := 0;
  name    text;
  is_time boolean;
BEGIN
  -- Serialise creators: two instances racing on CREATE ... PARTITION OF would conflict.
  PERFORM pg_advisory_xact_lock(hashtext('fleet_ensure_partitions'));
  -- Telemetry tables are partitioned on a timestamptz column, the rest on a date.
  SELECT format_type(a.atttypid, a.atttypmod) = 'timestamp with time zone'
    INTO is_time
    FROM pg_partitioned_table p
    JOIN pg_attribute a ON a.attrelid = p.partrelid AND a.attnum = p.partattrs[0]
   WHERE p.partrelid = parent;
  WHILE d <= to_day LOOP
    name := format('%s_p%s', parent::text, to_char(d, 'YYYYMMDD'));
    IF to_regclass(name) IS NULL THEN
      IF is_time THEN
        EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF %s FOR VALUES FROM (%L) TO (%L)',
                       name, parent::text, (d::timestamp AT TIME ZONE 'UTC'),
                       ((d + 1)::timestamp AT TIME ZONE 'UTC'));
      ELSE
        EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF %s FOR VALUES FROM (%L) TO (%L)',
                       name, parent::text, d, d + 1);
      END IF;
      created := created + 1;
    END IF;
    d := d + 1;
  END LOOP;
  RETURN created;
END $$;

SELECT fleet_ensure_partitions('agent_events', (now() AT TIME ZONE 'UTC')::date - 1, (now() AT TIME ZONE 'UTC')::date + 7);
SELECT fleet_ensure_partitions('step_executions', (now() AT TIME ZONE 'UTC')::date - 1, (now() AT TIME ZONE 'UTC')::date + 7);
