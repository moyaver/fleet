# ADR-0008: CI publisher authentication and untrusted PRs

**Status:** Accepted (shipped in M3, 2026-11-06) · **Date:** 2026-11-03 · **Research:** [06 §1–2, §6](../../research/06-ci-security-deployment.md)

## Decision
- **Token exchange:** the agent exchanges a GitHub Actions OIDC token (custom audience) for a short-lived, write-only Fleet token scoped to one project and session.
- **Verification:** JWKS cached and refetched on an unknown `kid`. Authorize on **immutable numeric claims** (`repository_id`, `repository_owner_id`), never on `sub` patterns; the `sub` format changed for repos created or renamed after 2026-07-15.
- **Identity from claims:** pipeline and job identity come from verified claims (`run_id`, `run_attempt`, `check_run_id`). CI environment variables are display-only.
- **Fallback credential:** prefix-identifiable API keys, stored as SHA-256 hashes, scoped to one project, write-only.
- **Trust levels:** `trusted` / `pr` / `untrusted`.
  - **Fork PRs run offline:** Maestro runs, the exit code is kept, a local bundle is written, nothing is published.
  - An optional `workflow_run` relay can publish that bundle labelled `untrusted`.
  - `pull_request_target` is unsupported.
- **Redaction** in both the agent and the server covers `-e` values, `MAESTRO_*` values, secret-named env vars, gitleaks-style patterns, and resolved values in `commands.json` and `maestro.log`.

## Consequences
GHES lacks `check_run_id`, so it needs a synthetic job key with an `identity_quality` flag.
