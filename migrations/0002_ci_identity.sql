-- SPDX-License-Identifier: AGPL-3.0-only
-- 0002 CI identity (research 05 §3.1, §6.1–§6.3, §9.2; ADR-0013 plans).

-- One CI pipeline run, by natural key. GitHub: "<repository>:<run_id>".
CREATE TABLE pipeline_runs (
  id                uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id        uuid NOT NULL REFERENCES projects,
  provider          text NOT NULL,
  provider_run_key  text NOT NULL,
  repository        text,
  commit_sha        text,
  git_ref           text,
  created_at        timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, provider, provider_run_key)
);
CREATE INDEX pipeline_runs_project_created ON pipeline_runs (project_id, created_at DESC);

-- A re-run is a new attempt; attempts never merge.
CREATE TABLE pipeline_attempts (
  id              uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id      uuid NOT NULL REFERENCES projects,
  pipeline_run_id uuid NOT NULL REFERENCES pipeline_runs,
  attempt         integer NOT NULL CHECK (attempt >= 1),
  first_seen_at   timestamptz NOT NULL DEFAULT now(),
  -- Live-view version: bumped in every ingest transaction that changes this attempt, so browser
  -- deltas are ordered and survive restarts (research 03 §5.1).
  version         bigint NOT NULL DEFAULT 0,
  UNIQUE (pipeline_run_id, attempt)
);

-- One CI job. Lifecycle and outcome are separate (research 05 §6.3).
CREATE TABLE jobs (
  id                  uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id          uuid NOT NULL REFERENCES projects,
  pipeline_attempt_id uuid NOT NULL REFERENCES pipeline_attempts,
  job_key             text NOT NULL,
  name                text,
  lifecycle           text NOT NULL DEFAULT 'starting'
                      CHECK (lifecycle IN ('pending', 'starting', 'running', 'finalizing', 'closed')),
  outcome             text CHECK (outcome IN ('completed', 'failed', 'cancelled', 'interrupted', 'unknown')),
  outcome_source      source_t,
  reopened_count      integer NOT NULL DEFAULT 0,
  started_at          timestamptz NOT NULL DEFAULT now(),
  closed_at           timestamptz,
  rev                 bigint NOT NULL DEFAULT 0,
  UNIQUE (pipeline_attempt_id, job_key),
  -- Outcome only once closed; a closed job always has one.
  CHECK ((lifecycle = 'closed') = (outcome IS NOT NULL))
);
CREATE INDEX jobs_open ON jobs (project_id, lifecycle) WHERE lifecycle <> 'closed';

-- The machine (research 05 §6.1). Identity is the runner name plus the agent's OS and arch.
CREATE TABLE workers (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id    uuid NOT NULL REFERENCES projects,
  identity_key  text NOT NULL,
  runner_name   text,
  os            text,
  arch          text,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, identity_key)
);

-- Immutable Fleet Planner manifests (ADR-0013, issue #21).
CREATE TABLE plans (
  project_id          uuid NOT NULL REFERENCES projects,
  plan_id             text NOT NULL CHECK (plan_id ~ '^[A-Za-z0-9._:-]{1,128}$'),
  pipeline_attempt_id uuid REFERENCES pipeline_attempts,
  manifest            jsonb NOT NULL,
  manifest_sha256     sha256_t NOT NULL,
  planned_total       integer NOT NULL CHECK (planned_total >= 0),
  created_at          timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (project_id, plan_id)
);

CREATE TABLE plan_shards (
  project_id  uuid NOT NULL,
  plan_id     text NOT NULL,
  shard_id    integer NOT NULL CHECK (shard_id >= 0),
  flows       text[] NOT NULL,
  estimate_ms bigint,
  PRIMARY KEY (project_id, plan_id, shard_id),
  FOREIGN KEY (project_id, plan_id) REFERENCES plans
);

-- One agent process (spec: agent session + ingestion cursor). Owns the `seq` space.
CREATE TABLE agent_runs (
  id                    uuid PRIMARY KEY,
  project_id            uuid NOT NULL REFERENCES projects,
  job_id                uuid NOT NULL REFERENCES jobs,
  worker_id             uuid REFERENCES workers,
  token_id              uuid REFERENCES agent_tokens,
  api_key_id            uuid REFERENCES api_keys,
  previous_agent_run_id uuid,
  trust_level           trust_level_t NOT NULL,
  -- Server UTC date at the first hello: the partition key of this run's journal and steps.
  run_day               date NOT NULL,
  agent_version         text NOT NULL,
  protocol_version      smallint NOT NULL,
  argv                  jsonb,
  state                 text NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active', 'detached', 'finalizing', 'ended', 'abandoned')),
  end_reason            text,
  late_resume           boolean NOT NULL DEFAULT false,
  -- Connection fencing (research 05 §3.7).
  epoch                 integer NOT NULL DEFAULT 0 CHECK (epoch >= 0),
  -- Journal cursor: every seq ≤ committed_seq is applied, in the same transaction.
  committed_seq         bigint NOT NULL DEFAULT 0 CHECK (committed_seq >= 0),
  end_seq               bigint,
  telemetry_committed   bigint NOT NULL DEFAULT 0,
  signal_received       integer,
  runner_exit_code     integer,
  runner_exit_signal   integer,
  plan_id               text,
  plan_shard_id         integer,
  started_at            timestamptz NOT NULL DEFAULT now(),
  last_frame_at         timestamptz NOT NULL DEFAULT now(),
  detached_at           timestamptz,
  ended_at              timestamptz,
  rev                   bigint NOT NULL DEFAULT 0
  -- plan_id is not a foreign key: a run may report its plan before (or without) the plan
  -- being published; the link is made by (project_id, plan_id) when both exist.
);
CREATE INDEX agent_runs_job ON agent_runs (job_id);
CREATE INDEX agent_runs_sweep ON agent_runs (state, last_frame_at)
  WHERE state IN ('active', 'detached', 'finalizing');

-- One WebSocket (or batch POST) per row: logs and fencing history.
CREATE TABLE connections (
  id           uuid PRIMARY KEY DEFAULT uuidv7(),
  agent_run_id uuid NOT NULL REFERENCES agent_runs,
  epoch        integer NOT NULL,
  transport    text NOT NULL CHECK (transport IN ('websocket', 'batch')),
  opened_at    timestamptz NOT NULL DEFAULT now(),
  closed_at    timestamptz,
  close_reason text
);
CREATE INDEX connections_run ON connections (agent_run_id);
