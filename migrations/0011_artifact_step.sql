-- SPDX-License-Identifier: AGPL-3.0-only
-- The step of the flow that produced an artifact (issue #76): the dashboard links a failure
-- screenshot to its step by this index, not by file name. Nullable: older artifacts and files
-- that no step produced have none. Same index space as step_executions.step_index.
ALTER TABLE artifacts ADD COLUMN step_index integer CHECK (step_index >= 0);
