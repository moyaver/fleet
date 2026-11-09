-- SPDX-License-Identifier: AGPL-3.0-only
-- The failing step's UI hierarchy joins the artifact allowlist (issue #74): Maestro's
-- `screen-hierarchy/step-NNN-….json`, with on-screen text redacted by the agent before upload.
-- Widening the CHECK keeps every existing row valid.
ALTER TABLE artifacts DROP CONSTRAINT artifacts_class_check;
ALTER TABLE artifacts ADD CONSTRAINT artifacts_class_check
  CHECK (class IN ('failure_screenshot', 'failure_hierarchy', 'commands', 'junit',
                   'runner_log', 'html_report'));
