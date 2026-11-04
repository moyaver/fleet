-- SPDX-License-Identifier: AGPL-3.0-only
-- 0001 tenancy and auth (PLAN §9, research 05 §9.2 and §13, ADR-0008). Forward-only.
-- Requires PostgreSQL 18 (uuidv7()).

-- ============================================================ conventions
-- Enums are text domains with a CHECK: one constraint swap adds a value (research 05 §9.1).
CREATE DOMAIN outcome_t AS text
  CHECK (VALUE IN ('passed', 'failed', 'skipped', 'interrupted', 'unknown'));
CREATE DOMAIN finality_t AS text
  CHECK (VALUE IN ('provisional', 'verified', 'unverified_final', 'inferred'));
CREATE DOMAIN source_t AS text
  CHECK (VALUE IN ('declared', 'agent_observed', 'process_exit', 'live_console', 'debug_log',
                   'artifact_file', 'junit_final', 'server_inferred'));
CREATE DOMAIN sha256_t AS bytea CHECK (octet_length(VALUE) = 32);
CREATE DOMAIN trust_level_t AS text CHECK (VALUE IN ('trusted', 'pr', 'untrusted'));
CREATE DOMAIN role_t AS text CHECK (VALUE IN ('viewer', 'maintainer', 'admin'));

-- ============================================================ tenancy
CREATE TABLE projects (
  id             uuid PRIMARY KEY DEFAULT uuidv7(),
  slug           text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
  name           text NOT NULL,
  default_branch text NOT NULL DEFAULT 'main',
  -- Policy: resume grace, artifact allowlist, metric interval (research 05 §3.6).
  settings       jsonb NOT NULL DEFAULT '{}',
  created_at     timestamptz NOT NULL DEFAULT now(),
  archived_at    timestamptz
);

CREATE TABLE users (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  oidc_issuer   text NOT NULL,
  oidc_subject  text NOT NULL,
  email         text,
  display_name  text,
  is_admin      boolean NOT NULL DEFAULT false,
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_login_at timestamptz,
  UNIQUE (oidc_issuer, oidc_subject)
);

-- A user's role on one project (NULL = every project).
CREATE TABLE role_bindings (
  id         uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id uuid REFERENCES projects,
  user_id    uuid NOT NULL REFERENCES users,
  role       role_t NOT NULL,
  created_by uuid REFERENCES users,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE NULLS NOT DISTINCT (project_id, user_id)
);

-- IdP group → role, configured per project (PLAN §7.5).
CREATE TABLE group_mappings (
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id  uuid REFERENCES projects,
  oidc_issuer text NOT NULL,
  group_claim text NOT NULL,
  role        role_t NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE NULLS NOT DISTINCT (project_id, oidc_issuer, group_claim)
);

-- Who may publish into a project with a CI OIDC token. Matches immutable numeric claims, never
-- `sub` (ADR-0008, research 06 §2.5).
CREATE TABLE publisher_trust_policies (
  id                  uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id          uuid NOT NULL REFERENCES projects,
  name                text NOT NULL,
  issuer              text NOT NULL,
  audience            text NOT NULL,
  repository_id       text NOT NULL,
  repository_owner_id text NOT NULL,
  -- Refs that count as trusted (globs); empty = none, so pushes are `pr` at best.
  trusted_refs        text[] NOT NULL DEFAULT '{}',
  -- Events accepted at all; empty = the default set (research 06 §2.6).
  allowed_events      text[] NOT NULL DEFAULT '{}',
  job_workflow_ref    text,
  environment         text,
  pr_policy           text NOT NULL DEFAULT 'restricted' CHECK (pr_policy IN ('restricted', 'full', 'deny')),
  created_at          timestamptz NOT NULL DEFAULT now(),
  revoked_at          timestamptz,
  UNIQUE (project_id, name)
);

-- Fallback credential for generic CI: `mf_live_…`, stored as a SHA-256 hash, one project,
-- write-only (PLAN §7.5).
CREATE TABLE api_keys (
  id           uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id   uuid NOT NULL REFERENCES projects,
  name         text NOT NULL,
  prefix       text NOT NULL,
  token_hash   sha256_t NOT NULL UNIQUE,
  trust_level  trust_level_t NOT NULL DEFAULT 'trusted',
  expires_at   timestamptz,
  revoked_at   timestamptz,
  created_by   uuid REFERENCES users,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz
);

-- Short-lived opaque agent tokens minted by POST /api/v1/agent/token. Hash only.
CREATE TABLE agent_tokens (
  id           uuid PRIMARY KEY DEFAULT uuidv7(),
  project_id   uuid NOT NULL REFERENCES projects,
  token_hash   sha256_t NOT NULL UNIQUE,
  policy_id    uuid REFERENCES publisher_trust_policies,
  api_key_id   uuid REFERENCES api_keys,
  trust_level  trust_level_t NOT NULL,
  -- Verified CI claims (repository, run_id, run_attempt, check_run_id, ...): these override what
  -- the agent reports in `hello` (research 06 §2.5 step 8).
  claims       jsonb NOT NULL DEFAULT '{}',
  expires_at   timestamptz NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz,
  CHECK (num_nonnulls(policy_id, api_key_id) = 1)
);
CREATE INDEX agent_tokens_expires_at ON agent_tokens (expires_at);

-- OIDC replay protection: each `jti` is accepted once while its token is valid.
CREATE TABLE oidc_jti_seen (
  issuer     text NOT NULL,
  jti        text NOT NULL,
  expires_at timestamptz NOT NULL,
  PRIMARY KEY (issuer, jti)
);

-- Browser sessions (M4 fills them; the table belongs to tenancy).
CREATE TABLE sessions (
  id         text PRIMARY KEY,
  user_id    uuid REFERENCES users,
  data       jsonb NOT NULL DEFAULT '{}',
  expires_at timestamptz NOT NULL
);
CREATE INDEX sessions_expires_at ON sessions (expires_at);

-- Append-only.
CREATE TABLE audit_events (
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  at          timestamptz NOT NULL DEFAULT now(),
  actor_kind  text NOT NULL CHECK (actor_kind IN ('user', 'publisher', 'system')),
  actor_id    uuid,
  project_id  uuid,
  action      text NOT NULL,
  target_kind text,
  target_id   uuid,
  details     jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_events_project_at ON audit_events (project_id, at DESC);
