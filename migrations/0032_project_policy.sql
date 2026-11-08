-- Screen G's per-project policy (#139). The configuration sets the server-wide values; a project's
-- override here replaces them for that project. A NULL override means "the configured value".
-- An artifact override can only tighten a server-wide size cap, which the server checks before it
-- writes (the database checks only that the value is positive or in range).
--
-- `source` says who created the project: `config` (the start-up sync names it, so the dashboard
-- does not rename it) or `dashboard`.
ALTER TABLE projects
  ADD COLUMN source text NOT NULL DEFAULT 'dashboard' CHECK (source IN ('config', 'dashboard')),
  ADD COLUMN artifact_bytes_per_day bigint CHECK (artifact_bytes_per_day > 0),
  ADD COLUMN artifact_files_per_day integer CHECK (artifact_files_per_day > 0),
  ADD COLUMN sample_interval_ms integer CHECK (sample_interval_ms BETWEEN 1000 AND 300000),
  ADD COLUMN pressure_sample_interval_ms integer
    CHECK (pressure_sample_interval_ms BETWEEN 1000 AND 300000);

-- Per-class overrides of the upload policy. A row says whether the class is accepted and its
-- largest file; a class without a row takes the configured values. Resetting a class deletes its row.
CREATE TABLE project_artifact_classes (
  project_id uuid NOT NULL REFERENCES projects,
  class      text NOT NULL CHECK (class IN ('failure_screenshot', 'failure_hierarchy', 'commands',
                                            'junit', 'maestro_log', 'html_report', 'device_logcat')),
  accepted   boolean NOT NULL,
  max_bytes  bigint CHECK (max_bytes > 0),
  PRIMARY KEY (project_id, class)
);
