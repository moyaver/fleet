-- SPDX-License-Identifier: AGPL-3.0-only
-- Maestro's own `takeScreenshot` images and `startRecording` videos join the artifact allowlist
-- (issue #133). They are uploaded only when the agent's user-media switch is on, and only for a
-- failed flow; their pixels are never redacted, so each class is its own, with its own size cap
-- (`artifacts.max_bytes.<class>`), its own retention class and its own per-project switch.
--
-- Every class CHECK names the two new classes, and so does the per-project override table (0032).
-- The retention class `user_media` is seeded for every project that exists and for each one
-- created later (the trigger from 0008 is replaced, not duplicated).

ALTER TABLE artifacts DROP CONSTRAINT artifacts_class_check;
ALTER TABLE artifacts ADD CONSTRAINT artifacts_class_check
  CHECK (class IN ('failure_screenshot', 'failure_hierarchy', 'commands', 'junit',
                   'runner_log', 'html_report', 'device_logcat', 'user_screenshot',
                   'user_recording'));

ALTER TABLE project_artifact_classes DROP CONSTRAINT project_artifact_classes_class_check;
ALTER TABLE project_artifact_classes ADD CONSTRAINT project_artifact_classes_class_check
  CHECK (class IN ('failure_screenshot', 'failure_hierarchy', 'commands', 'junit', 'runner_log',
                   'html_report', 'device_logcat', 'user_screenshot', 'user_recording'));

-- The retention class of both. Fourteen days, as for `video` and `bundle`: the media is large and
-- unredacted, so its default keeps it no longer than the other unredacted evidence.
ALTER TABLE retention_classes DROP CONSTRAINT retention_classes_class_check;
ALTER TABLE retention_classes ADD CONSTRAINT retention_classes_class_check
  CHECK (class IN ('report', 'failure_evidence', 'success_evidence', 'debug_log', 'video',
                   'bundle', 'user_media'));

CREATE OR REPLACE FUNCTION fleet_seed_retention_classes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO retention_classes (project_id, class, ttl) VALUES
    (NEW.id, 'report',           interval '365 days'),
    (NEW.id, 'failure_evidence', interval '90 days'),
    (NEW.id, 'success_evidence', interval '7 days'),
    (NEW.id, 'debug_log',        interval '30 days'),
    (NEW.id, 'video',            interval '14 days'),
    (NEW.id, 'bundle',           interval '14 days'),
    (NEW.id, 'user_media',       interval '14 days')
  ON CONFLICT DO NOTHING;
  RETURN NEW;
END $$;

INSERT INTO retention_classes (project_id, class, ttl)
  SELECT p.id, 'user_media', interval '14 days' FROM projects p
  ON CONFLICT DO NOTHING;
