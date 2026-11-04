-- SPDX-License-Identifier: AGPL-3.0-only
-- 0009 rollup bookkeeping (PLAN §7.7, §9; research 05 §10.2; issue #45). The rollup job fills
-- host_rollup_* and device_rollup_* (0005) from the raw samples, one window at a time. Two tables
-- let it be idempotent and late-data aware:
--   * rollup_watermarks: how far each 1-minute rollup has been computed (everything before it);
--   * rollup_dirty: minute buckets that received samples after the job (could have) passed them.

CREATE TABLE rollup_watermarks (
  name      text PRIMARY KEY CHECK (name IN ('host_1m', 'device_1m')),
  watermark timestamptz NOT NULL
);

-- `subject_id` is the agent run (host samples) or the device session (device samples).
CREATE TABLE rollup_dirty (
  source     text NOT NULL CHECK (source IN ('host', 'device')),
  subject_id uuid NOT NULL,
  bucket     timestamptz NOT NULL,
  PRIMARY KEY (source, subject_id, bucket)
);
CREATE INDEX rollup_dirty_bucket ON rollup_dirty (source, bucket);

-- A sample that is more than a minute old when it is inserted (a replay after a reconnect, a
-- spool upload) may land in a bucket that was already rolled up: remember its minute. The margin
-- is below the job's two-minute lag, so a sample the job cannot have seen yet is never missed
-- (rows inside the job's next window are re-rolled anyway; the extra mark only costs one
-- recomputation). Row triggers on a partitioned table are cloned onto every partition, so
-- partitions created later carry them. Only late rows do any work: the WHEN clause skips the rest.
CREATE FUNCTION fleet_mark_host_dirty() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO rollup_dirty (source, subject_id, bucket)
  VALUES ('host', NEW.agent_run_id, date_bin('1 minute', NEW.ts, TIMESTAMPTZ '2000-01-01'))
  ON CONFLICT DO NOTHING;
  RETURN NULL;
END $$;

CREATE FUNCTION fleet_mark_device_dirty() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO rollup_dirty (source, subject_id, bucket)
  VALUES ('device', NEW.device_session_id, date_bin('1 minute', NEW.ts, TIMESTAMPTZ '2000-01-01'))
  ON CONFLICT DO NOTHING;
  RETURN NULL;
END $$;

CREATE TRIGGER host_samples_late AFTER INSERT ON host_samples
  FOR EACH ROW WHEN (NEW.ts < clock_timestamp() - interval '1 minute')
  EXECUTE FUNCTION fleet_mark_host_dirty();

CREATE TRIGGER device_samples_late AFTER INSERT ON device_samples
  FOR EACH ROW WHEN (NEW.ts < clock_timestamp() - interval '1 minute')
  EXECUTE FUNCTION fleet_mark_device_dirty();
