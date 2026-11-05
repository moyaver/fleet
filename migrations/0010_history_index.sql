-- SPDX-License-Identifier: AGPL-3.0-only
-- History pages seek finished executions by project and their displayed timestamp.
CREATE INDEX flow_executions_history ON flow_executions
  (project_id, (coalesce(ended_at, started_at)) DESC, id DESC)
  WHERE state NOT IN ('pending', 'running');
