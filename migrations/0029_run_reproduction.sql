-- SPDX-License-Identifier: AGPL-3.0-only
-- The rerun command words of an agent run (issue #82): the redacted words of its `maestro test`
-- line, built by the agent, that the server joins with a flow's own path. Null for a run that
-- recorded none (an agent before this, or a run that never started a command).
ALTER TABLE agent_runs
    ADD COLUMN reproduction jsonb;
