-- SPDX-License-Identifier: AGPL-3.0-only
-- 0008 artifacts (PLAN §7.6, §9; research 05 §9, §12; ADR-0007). Blobs are content-addressed per
-- project; an artifact is one agent run's claim to a blob, with a class, a flow and a retention
-- class. Uploads are relayed through the server, so an artifact row exists from the first
-- `upload_request`, and only reaches `available` after the bytes passed verification.

-- Stored content: one row per (project, SHA-256), written once the blob store holds it.
-- `unreferenced_since` and `deleting` belong to the garbage collector (research 05 §12.4).
CREATE TABLE blobs (
  project_id         uuid NOT NULL REFERENCES projects,
  sha256             sha256_t NOT NULL,
  size_bytes         bigint NOT NULL CHECK (size_bytes >= 0),
  state              text NOT NULL DEFAULT 'present' CHECK (state IN ('present', 'deleting')),
  created_at         timestamptz NOT NULL DEFAULT now(),
  verified_at        timestamptz NOT NULL DEFAULT now(),
  unreferenced_since timestamptz,
  PRIMARY KEY (project_id, sha256)
);
CREATE INDEX blobs_unreferenced ON blobs (unreferenced_since) WHERE unreferenced_since IS NOT NULL;

-- How long each retention class lives; NULL keeps it until the project is deleted. Seeded for
-- every project (a trigger covers projects created later); per-project overrides arrive with the
-- admin UI.
CREATE TABLE retention_classes (
  project_id uuid NOT NULL REFERENCES projects,
  class      text NOT NULL CHECK (class IN ('report', 'failure_evidence', 'success_evidence',
                                             'debug_log', 'video', 'bundle')),
  ttl        interval,
  PRIMARY KEY (project_id, class)
);

CREATE FUNCTION fleet_seed_retention_classes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO retention_classes (project_id, class, ttl) VALUES
    (NEW.id, 'report',           interval '365 days'),
    (NEW.id, 'failure_evidence', interval '90 days'),
    (NEW.id, 'success_evidence', interval '7 days'),
    (NEW.id, 'debug_log',        interval '30 days'),
    (NEW.id, 'video',            interval '14 days'),
    (NEW.id, 'bundle',           interval '14 days')
  ON CONFLICT DO NOTHING;
  RETURN NEW;
END $$;

CREATE TRIGGER projects_seed_retention AFTER INSERT ON projects
  FOR EACH ROW EXECUTE FUNCTION fleet_seed_retention_classes();

INSERT INTO retention_classes (project_id, class, ttl)
  SELECT p.id, c.class, c.ttl
    FROM projects p,
         (VALUES ('report', interval '365 days'), ('failure_evidence', interval '90 days'),
                 ('success_evidence', interval '7 days'), ('debug_log', interval '30 days'),
                 ('video', interval '14 days'), ('bundle', interval '14 days')) AS c (class, ttl)
  ON CONFLICT DO NOTHING;

CREATE TABLE artifacts (
  id                 uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id         uuid NOT NULL REFERENCES projects,
  agent_run_id       uuid NOT NULL REFERENCES agent_runs,
  job_id             uuid NOT NULL REFERENCES jobs,
  suite_execution_id uuid REFERENCES suite_executions,
  flow_execution_id  uuid REFERENCES flow_executions,
  -- The allowlist (PLAN §6.6): a class not named here cannot be stored.
  class              text NOT NULL CHECK (class IN ('failure_screenshot', 'commands', 'junit',
                                                    'maestro_log', 'html_report')),
  -- What the uploader declared. What is served is `served_type`, from the verified content.
  content_type       text NOT NULL,
  served_type        text,
  size_bytes         bigint NOT NULL CHECK (size_bytes >= 0),
  sha256             sha256_t NOT NULL,
  -- A sanitised label relative to the run's output directory, and its last segment.
  path               text NOT NULL,
  name               text NOT NULL,
  retention_class    text NOT NULL,
  expires_at         timestamptz,
  legal_hold         boolean NOT NULL DEFAULT false,
  -- declared: a grant is out; uploading: bytes are arriving or sit in the spool awaiting
  -- `complete`; available: verified and stored; rejected: refused, nothing kept.
  state              text NOT NULL DEFAULT 'declared'
                     CHECK (state IN ('declared', 'uploading', 'available', 'rejected', 'expired')),
  reject_reason      text,
  -- The grant: who it was issued to (an API key or agent token id) and when it lapses.
  credential_id      uuid,
  grant_expires_at   timestamptz,
  received_at        timestamptz,
  created_at         timestamptz NOT NULL DEFAULT now(),
  available_at       timestamptz,
  -- Idempotent upload requests: the same file in the same run is one artifact.
  UNIQUE (agent_run_id, class, sha256),
  FOREIGN KEY (project_id, retention_class) REFERENCES retention_classes (project_id, class),
  CHECK (state <> 'available' OR (served_type IS NOT NULL AND available_at IS NOT NULL)),
  CHECK (state <> 'rejected' OR reject_reason IS NOT NULL)
);
CREATE INDEX artifacts_flow ON artifacts (flow_execution_id) WHERE flow_execution_id IS NOT NULL;
CREATE INDEX artifacts_job ON artifacts (job_id);
-- The garbage collector's reference check, and upload deduplication.
CREATE INDEX artifacts_blob ON artifacts (project_id, sha256);
CREATE INDEX artifacts_expiry ON artifacts (expires_at) WHERE state = 'available' AND NOT legal_hold;
