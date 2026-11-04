-- SPDX-License-Identifier: AGPL-3.0-only
-- 0003 execution (research 05 §6.4–§6.7, §7, §9.2). Entity IDs are agent-generated, so a replay
-- upserts the same rows; every entity records the agent run that owns it (ownership check, §9.3b).

-- The comparable kind of device; filled from adb properties in a later milestone.
CREATE TABLE device_profiles (
  id             uuid PRIMARY KEY DEFAULT uuidv7(),
  platform       text NOT NULL CHECK (platform IN ('android', 'ios')),
  kind           text NOT NULL CHECK (kind IN ('emulator', 'simulator', 'physical')),
  model          text,
  avd_or_runtime text,
  os_version     text,
  api_level      smallint,
  abi            text,
  fingerprint    sha256_t NOT NULL UNIQUE,
  extra          jsonb NOT NULL DEFAULT '{}'
);

-- One device as seen by one agent run. The serial is an attribute, not an identity.
CREATE TABLE device_sessions (
  id            uuid PRIMARY KEY,
  project_id    uuid NOT NULL REFERENCES projects,
  agent_run_id  uuid NOT NULL REFERENCES agent_runs,
  profile_id    uuid REFERENCES device_profiles,
  serial        text NOT NULL,
  state         text NOT NULL DEFAULT 'discovered'
                CHECK (state IN ('discovered', 'booting', 'ready', 'busy', 'offline', 'released', 'lost')),
  first_seen_at timestamptz NOT NULL,
  last_seen_at  timestamptz NOT NULL,
  rev           bigint NOT NULL DEFAULT 0
);
CREATE INDEX device_sessions_run ON device_sessions (agent_run_id);

-- One Maestro invocation.
CREATE TABLE suite_executions (
  id             uuid PRIMARY KEY,
  project_id     uuid NOT NULL REFERENCES projects,
  agent_run_id   uuid NOT NULL REFERENCES agent_runs,
  job_id         uuid NOT NULL REFERENCES jobs,
  shard_mode     text NOT NULL CHECK (shard_mode IN ('none', 'split', 'all')),
  shard_count    integer NOT NULL DEFAULT 1 CHECK (shard_count >= 0),
  -- NULL = plan unknown: progress is indeterminate, never guessed.
  planned_count  integer,
  plan_source    source_t,
  lifecycle      text NOT NULL DEFAULT 'starting'
                 CHECK (lifecycle IN ('starting', 'running', 'reporting', 'closed')),
  outcome        text CHECK (outcome IN ('passed', 'failed', 'interrupted', 'unknown')),
  outcome_source source_t,
  report_status  text CHECK (report_status IN ('complete', 'partial', 'absent', 'invalid')),
  exit_code      integer,
  exit_signal    integer,
  exit_class     text,
  started_at     timestamptz,
  ended_at       timestamptz,
  rev            bigint NOT NULL DEFAULT 0,
  -- Invariant I3 at suite level: `passed` needs exit status 0.
  CHECK (outcome IS DISTINCT FROM 'passed' OR exit_code = 0)
);
CREATE INDEX suite_executions_job ON suite_executions (job_id);

CREATE TABLE shards (
  id                 uuid PRIMARY KEY,
  project_id         uuid NOT NULL REFERENCES projects,
  agent_run_id       uuid NOT NULL REFERENCES agent_runs,
  suite_execution_id uuid NOT NULL REFERENCES suite_executions,
  shard_index        integer NOT NULL CHECK (shard_index >= 1),
  device_session_id  uuid REFERENCES device_sessions,
  planned            jsonb NOT NULL DEFAULT '[]',
  planned_count      integer,
  UNIQUE (suite_execution_id, shard_index)
);

-- A flow file, by repo-relative path. Linked when a source gives that path (the JUnit `file`).
CREATE TABLE flow_definitions (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id    uuid NOT NULL REFERENCES projects,
  repo_path     text NOT NULL,
  last_name     text,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, repo_path)
);

-- One execution of one flow. `state` is derived from flow_claims by the claim lattice
-- (fleet-core); it is never written from a single event.
CREATE TABLE flow_executions (
  id                  uuid PRIMARY KEY,
  project_id          uuid NOT NULL REFERENCES projects,
  agent_run_id        uuid NOT NULL REFERENCES agent_runs,
  suite_execution_id  uuid NOT NULL REFERENCES suite_executions,
  shard_id            uuid REFERENCES shards,
  device_session_id   uuid REFERENCES device_sessions,
  flow_definition_id  uuid REFERENCES flow_definitions,
  flow_name           text NOT NULL,
  flow_path           text,
  shard_number        integer,
  ordinal             integer,
  started             boolean NOT NULL DEFAULT false,
  state               text NOT NULL DEFAULT 'pending'
                      CHECK (state IN ('pending', 'running', 'passed', 'failed', 'skipped', 'interrupted', 'unknown')),
  outcome_source      source_t,
  finality            finality_t NOT NULL DEFAULT 'provisional',
  has_conflict        boolean NOT NULL DEFAULT false,
  failure_message     text,
  started_at          timestamptz,
  ended_at            timestamptz,
  duration_ms         bigint,
  -- Denormalised for scoping and analytics (immutable after insert).
  pipeline_attempt_id uuid NOT NULL REFERENCES pipeline_attempts,
  job_id              uuid NOT NULL REFERENCES jobs,
  rev                 bigint NOT NULL DEFAULT 0,
  -- Invariant I3, second line of defence: a passed flow's winning source is a parsed Maestro
  -- source (rank ≥ 30), never an inference.
  CHECK (state <> 'passed' OR outcome_source IN ('live_console', 'debug_log', 'artifact_file', 'junit_final'))
);
CREATE INDEX flow_executions_suite ON flow_executions (suite_execution_id);
CREATE INDEX flow_executions_shard ON flow_executions (shard_id, ordinal);
CREATE INDEX flow_executions_attempt ON flow_executions (pipeline_attempt_id);
CREATE INDEX flow_executions_recent_failures ON flow_executions (project_id, ended_at DESC)
  WHERE state = 'failed';

-- Evidence. A flow's outcome is resolve(claims): a set, so order and duplicates don't matter.
CREATE TABLE flow_claims (
  flow_execution_id uuid NOT NULL REFERENCES flow_executions,
  source            source_t NOT NULL,
  -- '<agent_run_id>:<seq>' for agent claims, 'sweep:<agent_run_id>' for server inferences.
  origin            text NOT NULL,
  outcome           outcome_t NOT NULL,
  -- Arrival order within the agent run (seq); breaks same-rank ties.
  claim_order       bigint NOT NULL,
  observed_at       timestamptz,
  detail            jsonb NOT NULL DEFAULT '{}',
  recorded_at       timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (flow_execution_id, source, origin),
  -- Invariant I3: inferred, exit-status and declared sources can never claim passed.
  CHECK (NOT (outcome = 'passed' AND source IN ('process_exit', 'agent_observed', 'server_inferred', 'declared')))
);

-- Steps: high volume, partitioned by the owning agent run's run_day.
CREATE TABLE step_executions (
  run_day           date NOT NULL,
  flow_execution_id uuid NOT NULL,
  step_index        integer NOT NULL CHECK (step_index >= 0),
  label             text,
  status            text NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'warned', 'skipped', 'unknown')),
  source            source_t NOT NULL,
  started_at        timestamptz,
  ended_at          timestamptz,
  PRIMARY KEY (run_day, flow_execution_id, step_index)
) PARTITION BY RANGE (run_day);
