-- SPDX-License-Identifier: AGPL-3.0-only
-- The step alignment warnings a flow's final `commands.json` raised (issue #148): steps the log
-- and the file could not line up (`steps_not_aligned`), and steps whose status they disagree on
-- (`step_status_disagrees`). Evidence about how verified the steps are, never the flow's verdict
-- (I3). Empty when the steps lined up.
ALTER TABLE flow_executions
    ADD COLUMN step_warnings text[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT flow_step_warnings_known CHECK (
        step_warnings <@ ARRAY['steps_not_aligned', 'step_status_disagrees']::text[]
    );
