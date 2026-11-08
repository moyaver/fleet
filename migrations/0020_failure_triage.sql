-- SPDX-License-Identifier: AGPL-3.0-only
-- Failure triage (issue #84; ADR-0009 keeps triage thin): whether a failure fingerprint is
-- known (with the issue that tracks it) or resolved. One row per fingerprint, created when the
-- fingerprint is first seen, so `first_seen_at` is recorded for every failure. `state` NULL means
-- untriaged. Every change is also written to `audit_events` in the same transaction.
CREATE TABLE failure_triage (
  project_id    uuid NOT NULL REFERENCES projects,
  fingerprint   text NOT NULL CHECK (fingerprint ~ '^v[0-9]+:[0-9a-f]{16}$'),
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  state         text CHECK (state IN ('known', 'resolved')),
  issue_url     text CHECK (issue_url ~ '^https?://[^[:space:]]+$' AND length(issue_url) <= 2000),
  updated_at    timestamptz,
  PRIMARY KEY (project_id, fingerprint),
  -- Known means "tracked by this issue", so it always names one. Untriaged clears the link.
  CHECK (state IS DISTINCT FROM 'known' OR issue_url IS NOT NULL),
  CHECK (state IS NOT NULL OR issue_url IS NULL)
);

-- Existing failures predate the table: their first sighting is the earliest computation.
INSERT INTO failure_triage (project_id, fingerprint, first_seen_at)
SELECT project_id, fingerprint, min(computed_at)
  FROM failure_fingerprints
 GROUP BY project_id, fingerprint;
