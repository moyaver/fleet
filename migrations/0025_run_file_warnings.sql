-- SPDX-License-Identifier: AGPL-3.0-only
-- The warnings an agent run's own files raised (issue #148): a final `commands.json` whose flow the
-- log never started (`commands_flow_unnamed`). Such a file has no flow to belong to, and it never
-- creates one (I3), so the warning is kept on the run. Empty when every file found its flow.
ALTER TABLE agent_runs
    ADD COLUMN file_warnings text[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT agent_run_file_warnings_known CHECK (
        file_warnings <@ ARRAY['commands_flow_unnamed']::text[]
    );
