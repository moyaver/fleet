-- SPDX-License-Identifier: AGPL-3.0-only
-- Two changes, one migration (research 06 T21 and T12).
--
-- When a browser session was last used. A session ends at its absolute lifetime (`expires_at`) or
-- after `login.session_idle_minutes` without a request. Existing sessions start their idle window
-- at this migration.
ALTER TABLE sessions ADD COLUMN last_seen_at timestamptz NOT NULL DEFAULT now();

-- The per-project daily upload quota counts a project's declarations of the last 24 hours, across
-- its runs, so it reads them by project and creation time.
CREATE INDEX artifacts_project_created ON artifacts (project_id, created_at);
