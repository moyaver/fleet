-- SPDX-License-Identifier: AGPL-3.0-only
-- The cut-short fact of a flow (issue #145): the agent's `commands_not_final` warning, which says
-- its final `commands.json` was incomplete. It is the recorded half of the device-loss evidence, and
-- it is kept on the flow, not read from the journal, because the journal is retained for 14 days
-- while a flow's history is kept for 90. Null when the flow's commands were not cut short. The
-- message is the agent's, redacted; the first warning of a flow is kept.
ALTER TABLE flow_executions ADD COLUMN cut_short text;

-- Backfill the flows whose warning the journal still holds. Older warnings are gone with their
-- journal partitions, which is the loss this column exists to prevent from now on.
UPDATE flow_executions f
   SET cut_short = w.message
  FROM (SELECT DISTINCT ON (e.flow_execution_id)
               e.flow_execution_id, e.data->>'message' AS message
          FROM agent_events e
         WHERE e.flow_execution_id IS NOT NULL
           AND e.type = 'agent.warning'
           AND e.data->>'code' = 'commands_not_final'
         ORDER BY e.flow_execution_id, e.run_day, e.agent_run_id, e.seq) w
 WHERE f.id = w.flow_execution_id
   AND w.message IS NOT NULL;
