-- SPDX-License-Identifier: AGPL-3.0-only
-- The flow catalog (issue #85): a flow's tags, from its header, as the agent found them in the
-- workspace. A tag is a fact of the flow file, so it lives on the flow, and a flow with no
-- executions yet can carry one. The agent bounds a flow to 16 tags of at most 64 bytes each.
ALTER TABLE flow_definitions
    ADD COLUMN tags text[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT flow_definition_tags_count CHECK (cardinality(tags) <= 16);
