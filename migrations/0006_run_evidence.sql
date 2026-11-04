-- SPDX-License-Identifier: AGPL-3.0-only
-- 0006 run evidence (#31): which evidence sources the agent's prepared command will produce
-- (JUnit report path or why none, debug output, machine-readable log), from `run.started`.
-- NULL: an older agent that didn't say.

ALTER TABLE agent_runs ADD COLUMN evidence jsonb;
