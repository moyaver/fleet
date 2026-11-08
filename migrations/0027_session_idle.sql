-- When a browser session was last used (research 06 T21). A session ends at its absolute
-- lifetime (`expires_at`) or after `login.session_idle_minutes` without a request. Existing
-- sessions start their idle window at this migration.
ALTER TABLE sessions ADD COLUMN last_seen_at timestamptz NOT NULL DEFAULT now();
