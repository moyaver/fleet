-- SPDX-License-Identifier: AGPL-3.0-only
-- 0007 analytics (ADR-0009; PLAN §9; research 05 §11.1). Written in the ingest transaction when
-- a flow's outcome becomes final; the algorithms live in fleet-core (`fingerprint`, `flaky`).
-- (0008 is the artifacts migration, owned by the M5 blob slice.)

-- The execution conditions of a flow: a SHA-256 over the canonical dimensions, which are kept in
-- `dims` so a coarser grouping can drop dimensions at query time. A fact the agent didn't report
-- is the string 'unknown' there, never a guess.
CREATE TABLE condition_fingerprints (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id    uuid NOT NULL REFERENCES projects,
  fingerprint   sha256_t NOT NULL,
  dims          jsonb NOT NULL,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, fingerprint)
);

-- Set with the flow's final outcome. NULL: not final yet, or not a pass or a failure.
ALTER TABLE flow_executions
  ADD COLUMN condition_fingerprint_id uuid REFERENCES condition_fingerprints;

-- "These failures are the same failure": one row per failed, final flow execution. The
-- fingerprint is versioned text (`v1:` + 16 hex digits); the normalized parts it was made from
-- are kept so a group can be shown without re-deriving it.
CREATE TABLE failure_fingerprints (
  flow_execution_id uuid PRIMARY KEY REFERENCES flow_executions ON DELETE CASCADE,
  project_id        uuid NOT NULL REFERENCES projects,
  fingerprint       text NOT NULL CHECK (fingerprint ~ '^v[0-9]+:[0-9a-f]{16}$'),
  step_kind         text NOT NULL,
  message           text NOT NULL,
  computed_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX failure_fingerprints_group ON failure_fingerprints (project_id, fingerprint);

-- The input of flaky v1 (fleet_core::flaky::classify): every final pass or failure with its
-- condition fingerprint, commit and branch kind. The classification itself is not SQL, so the
-- rule is written once. Infrastructure and host-anomaly exclusions come with failure
-- classification (not built yet); until then no run carries one.
CREATE VIEW flaky_history AS
SELECT f.project_id,
       coalesce(d.repo_path, f.flow_name)  AS flow,
       f.id                                AS flow_execution_id,
       pr.commit_sha,
       c.fingerprint                       AS condition_fingerprint,
       f.state,
       f.outcome_source,
       f.finality,
       -- A run whose ref is unknown is not counted as a default-branch run.
       coalesce(pr.git_ref IN (p.default_branch, 'refs/heads/' || p.default_branch), false)
                                           AS is_default_branch,
       f.ended_at
  FROM flow_executions f
  JOIN projects p              ON p.id = f.project_id
  JOIN pipeline_attempts pa    ON pa.id = f.pipeline_attempt_id
  JOIN pipeline_runs pr        ON pr.id = pa.pipeline_run_id
  JOIN condition_fingerprints c ON c.id = f.condition_fingerprint_id
  LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
 WHERE f.state IN ('passed', 'failed')
   AND f.finality IN ('verified', 'unverified_final')
   AND f.ended_at IS NOT NULL;
