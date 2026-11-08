-- SPDX-License-Identifier: AGPL-3.0-only
-- Where a run's evidence came from (issue #83). `live` runs were observed as they ran; `published`
-- runs were sent after the fact by `fleet-agent publish` (an offline bundle or a saved output) and
-- are shown as imported, never as live. Every run before this migration was live.
ALTER TABLE agent_runs
  ADD COLUMN origin text NOT NULL DEFAULT 'live' CHECK (origin IN ('live', 'published'));
