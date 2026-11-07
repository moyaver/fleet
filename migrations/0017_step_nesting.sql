-- SPDX-License-Identifier: AGPL-3.0-only
-- Where a step sits in its flow's nesting, and which retry attempt ran it (issue #38), as Maestro's
-- log shows them. All nullable: a top-level step has none of them, and older agents send none.
-- Same index space as step_executions.step_index; depth is at least 1 and attempt at least 1.
ALTER TABLE step_executions
    ADD COLUMN depth integer CHECK (depth >= 1),
    ADD COLUMN parent_step_index integer CHECK (parent_step_index >= 0),
    ADD COLUMN attempt integer CHECK (attempt >= 1);
