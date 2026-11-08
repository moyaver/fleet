-- SPDX-License-Identifier: AGPL-3.0-only
-- 0030 Screen G's writes (issue #139, part 2a). Forward-only.
--
-- A row from the running configuration is replaced by each start-up; a row added on the dashboard
-- is kept until someone edits or removes it there. `source` says which: `config` rows are
-- replaced by the sync and locked on the dashboard, `dashboard` rows never are by the sync.
ALTER TABLE group_mappings
  ADD COLUMN source text NOT NULL DEFAULT 'config' CHECK (source IN ('config', 'dashboard'));
ALTER TABLE publisher_trust_policies
  ADD COLUMN source text NOT NULL DEFAULT 'config' CHECK (source IN ('config', 'dashboard'));

-- A trust policy can be removed on the dashboard. Tokens minted from it stop with it: removal
-- expires them first, and then the row goes; their `policy_id` is cleared rather than blocking the
-- delete. A token keeps exactly one of its two sources, or (once its policy is removed) neither.
ALTER TABLE agent_tokens DROP CONSTRAINT agent_tokens_check;
ALTER TABLE agent_tokens DROP CONSTRAINT agent_tokens_policy_id_fkey;
ALTER TABLE agent_tokens
  ADD CONSTRAINT agent_tokens_policy_id_fkey
  FOREIGN KEY (policy_id) REFERENCES publisher_trust_policies ON DELETE SET NULL;
ALTER TABLE agent_tokens ADD CONSTRAINT agent_tokens_check CHECK (num_nonnulls(policy_id, api_key_id) <= 1);

-- The audit history sorts the whole table, newest first (Screen G, /admin/audit).
CREATE INDEX audit_events_at_id ON audit_events (at DESC, id DESC);
