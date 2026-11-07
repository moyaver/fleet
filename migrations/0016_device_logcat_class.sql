-- SPDX-License-Identifier: AGPL-3.0-only
-- The device logcat excerpt of a failed flow (issue #75) is an artifact class of its own. The
-- allowlist (PLAN §6.6) names every class a row may hold, so the check gains `device_logcat`.
-- Keeps every class 0015 allows: rewriting the list without `failure_hierarchy` would make the
-- next upload of a failing step's hierarchy fail its CHECK.
ALTER TABLE artifacts DROP CONSTRAINT artifacts_class_check;
ALTER TABLE artifacts ADD CONSTRAINT artifacts_class_check
  CHECK (class IN ('failure_screenshot', 'failure_hierarchy', 'commands', 'junit',
                   'maestro_log', 'html_report', 'device_logcat'));
