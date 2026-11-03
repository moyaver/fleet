# 06 — CI integration, runner environments, security model and self-hosted deployment

**Research date:** 2026-11-03
**Scope:** Spec v1.0 sections 10 (Security), 11 (Deployment), 12 (CI integration), 13 (Failure behaviour) and 16 (Acceptance criteria), re-examined for an all-Rust implementation: a Rust agent CLI and an axum server, with PostgreSQL and S3-compatible object storage, deployed self-hosted with Docker Compose.

**How claims are marked**

- **[V]**: verified on 2026-11-03 against the source cited (the number refers to the Sources section at the end).
- **[I]**: inferred, a design recommendation, or prior knowledge not re-verified today. Check these before relying on them.

---

## 0. Executive summary: findings that change or sharpen the spec

1. **GitHub now exposes the job instance ID. Use it as the job key.** The `job.check_run_id` expression context was added in September 2025 [V-3]. The `check_run_id` OIDC claim was added on 2025-11-13 [V-4]. That number is the `/job/<id>` segment of the job-log URL [V-3]. It is unique per matrix leg and per re-run attempt. It is not a default environment variable [V-1], so the workflow (or the setup action) must pass `${{ job.check_run_id }}` through the environment. It is not available on GHES [V-3], so GHES needs a fallback.
2. **The OIDC token can be the source of truth for CI metadata.** The token carries `repository_id`, `run_id`, `run_attempt`, `check_run_id`, `event_name`, `ref`, `runner_environment` and other claims [V-2]. The server should take identity and trust from the **verified claims**, not from environment variables the agent reports. The environment variables only enrich display.
3. **The OIDC `sub` format is changing.** Repositories created after 2026-07-15, and repositories renamed or transferred after that date, get immutable subjects of the form `repo:OWNER@OWNER-ID/REPO@REPO-ID:...` [V-5]. Fleet must **not** authorize on `sub` string patterns. Authorize on the numeric `repository_id` and `repository_owner_id` claims.
4. **Fork pull requests get no OIDC token and no secrets** [V-6, V-7, I], and spec §10 already forbids giving them privileged credentials. Recommended default: **an offline mode with no publishing**. The agent still wraps Maestro and preserves the exit code, and writes a local bundle. As an optional extra, a trusted `workflow_run` job can publish that bundle with an `untrusted` provenance label and a restricted artifact policy. **Never use `pull_request_target`** to run PR code [V-6, V-8].
5. **Maestro's `-e KEY=VALUE` flag is a confirmed credential vector.** Maestro's own docs show `-e PASSWORD=secret` [V-20]. Maestro also imports every `MAESTRO_*` environment variable into flows [V-20]. Redaction must cover argv values, `MAESTRO_*` values, and resolved values that later appear in Maestro debug JSON and logs.
6. **The MinIO community edition is not a viable default.** Its admin console was stripped in 2025 [V-24], binary and Docker image distribution stopped in October 2025 [V-25], and the repository was **archived on 2026-04-25** with the README saying "THIS REPOSITORY IS NO LONGER MAINTAINED" [V-23]. **Recommended default for the reference Compose deployment: Garage v2.5.0** (Rust, AGPLv3, released 2026-10-24). Use SeaweedFS (Apache-2.0) where AGPL is unacceptable. RustFS reached 1.0 GA only in September 2026 and had a CVSS 9.8 CVE in 2025, so it is a "watch" option. Fleet should also ship a built-in **filesystem backend**.
7. **No GitHub-hosted arm64 macOS runner can run the Android emulator.** Nested virtualization is unsupported there [V-14]. Linux x64 runners with KVM are the supported path [V-14, V-15]. Intel macOS images (`macos-15-intel`, `macos-26-intel`) still exist but are being retired around fall 2027 [I-17]. Self-hosted Apple Silicon Mac minis need arm64-v8a system images and use Hypervisor.framework (HVF) [V-16].
8. **The WebSocket ping interval must be at most about 25 s.** Common idle timeouts are nginx 60 s [V-30], AWS ALB 60 s [V-31], Traefik `readTimeout` 60 s [V-32] and Cloudflare about 100 s [V-33, I]. Caddy has no idle timeout but **closes WebSockets on config reload** unless `stream_close_delay` is set [V-34]. The agent's reconnect logic is mandatory, not optional.
9. **Use PostgreSQL 18 (18.6).** PG 19 was only at RC1 on 2026-11-01 [V-35, V-36]. The PG 18 Docker image moved `PGDATA` to `/var/lib/postgresql/18/docker` and the volume must be mounted at `/var/lib/postgresql` [V-37]. Old Compose snippets will break.
10. **Spec §4 and §6 stack recommendations (Python agent, Node.js server) are superseded** by the all-Rust decision. For this area, that means rustls with corporate CA support, explicit HTTP CONNECT proxy support for the WebSocket client, and a `fleet healthcheck` subcommand, because distroless images have no `curl`.

---

## 1. GitHub Actions adapter

### 1.1 Default environment variables → Fleet fields

Descriptions in the "Notes" column come from GitHub's variables reference [V-1] unless marked otherwise.

| Env var | Fleet field | Notes |
|---|---|---|
| `GITHUB_ACTIONS` (`true`) | adapter detection | Detection signal [I: standard]. `CI=true` is set too, but anything can set it [V-1]. |
| `GITHUB_SERVER_URL` | `ci.server_url` | `https://github.com`, a GHES host, or a `*.ghe.com` host. Part of every identity key, so two GitHub instances never collide. |
| `GITHUB_API_URL` | (agent-only) | Used only for the optional jobs-API fallback (§1.3). |
| `GITHUB_REPOSITORY` | `repo.full_name` (display) | `owner/name`. Mutable on rename, so not used as a key. |
| `GITHUB_REPOSITORY_ID` | `repo.provider_id` (key) | Numeric and immutable. Use it for project binding [V-1, V-2]. |
| `GITHUB_REPOSITORY_OWNER`, `GITHUB_REPOSITORY_OWNER_ID` | `repo.owner`, `repo.owner_id` | Owner ID is immutable [V-1]. |
| `GITHUB_WORKFLOW` | `pipeline.name` | Workflow `name:` or, if unset, the file path [V-1]. |
| `GITHUB_WORKFLOW_REF` | `pipeline.workflow_ref` | `owner/repo/.github/workflows/x.yml@refs/heads/main`. Groups history per workflow file. |
| `GITHUB_WORKFLOW_SHA` | `pipeline.workflow_sha` | Commit of the workflow file [V-1]. |
| `GITHUB_RUN_ID` | `pipeline.provider_run_id` | Unique per run within the repository and **unchanged on re-run** [V-1]. |
| `GITHUB_RUN_ATTEMPT` | `pipeline.attempt` | Starts at 1 and increments on each re-run [V-1]. Pipeline key = `(server_url, repository_id, run_id, run_attempt)`. Spec §7 requires that attempts be distinguished. |
| `GITHUB_RUN_NUMBER` | `pipeline.number` (display) | The "#142" shown in the UI. Per workflow, unchanged on re-run [V-1]. |
| `GITHUB_JOB` | `job.key` | The YAML **job key** (`jobs.<job_id>`), **not unique across matrix legs** [V-1]. Never use it alone as the job identity. |
| *(none)* → `${{ job.check_run_id }}` | `job.provider_job_id` | Numeric check-run ID of this job instance [V-3]. Must be passed explicitly (for example `FLEET_GITHUB_CHECK_RUN_ID`). It is also an OIDC claim [V-4]. Not on GHES [V-3]. |
| *(none)* → `${{ strategy.job-index }}`, `${{ strategy.job-total }}`, `${{ toJSON(matrix) }}` | `job.matrix_index`, `job.matrix_total`, `job.matrix` | Needed to label legs ("api 34 / smoke"). Pass them explicitly [I: contexts documented, V-2b partially]. |
| `GITHUB_SHA` | `commit.tested_sha` | **Depends on the event.** For `pull_request` it is the **merge commit** of the PR merge ref [V-6]. For `pull_request_target` it is the last commit on the default branch [V-6]. |
| *(event payload)* `pull_request.head.sha` | `commit.head_sha` | The commit developers recognise for a PR [V-6]. Store both SHAs and display `head_sha` for PRs. |
| *(event payload)* `pull_request.base.sha`, `pull_request.number`, `pull_request.head.repo.fork` | `commit.base_sha`, `pr.number`, `pr.is_fork` | Read from `GITHUB_EVENT_PATH` [V-1], extracting **only these fields**. Never upload the payload: it contains PR bodies, user data and more [I]. |
| `GITHUB_REF` | `ref.full` | For PRs this is `refs/pull/<n>/merge` [V-1]. |
| `GITHUB_REF_NAME` | `ref.name` | For PRs this is `<n>/merge` [V-1]. |
| `GITHUB_HEAD_REF` | `pr.head_branch` | Only set for `pull_request` and `pull_request_target` [V-1]. Use it as `branch` for PR runs. |
| `GITHUB_BASE_REF` | `pr.base_branch` | Same availability as `GITHUB_HEAD_REF` [V-1]. |
| `GITHUB_EVENT_NAME` | `pipeline.trigger` | Input to the trust policy (§2.6). |
| `GITHUB_ACTOR`, `GITHUB_ACTOR_ID`, `GITHUB_TRIGGERING_ACTOR` | `pipeline.actor` | Display only. These may be PII under company policy. Make collection configurable [I]. |
| `RUNNER_NAME` | `worker.name` | "May not be unique" [V-1]. Worker key = hash of `RUNNER_NAME` + host machine-id + boot id [I]. |
| `RUNNER_OS` / `RUNNER_ARCH` | `worker.os` / `worker.arch` | `Linux`/`Windows`/`macOS`; `X86`/`X64`/`ARM`/`ARM64` [V-1]. |
| `RUNNER_ENVIRONMENT` | `worker.kind` | `github-hosted` or `self-hosted` [V-1]. Also an OIDC claim [V-2]. |
| `RUNNER_TEMP` | (agent-only) | Spool directory for the offline buffer and bundles. Emptied at job start and end [V-1]. |
| `RUNNER_DEBUG` | (agent-only) | `1` means agent verbose logging [V-1]. |
| `ImageOS`, `ImageVersion` | `worker.image` | Set on GitHub-hosted images [I: prior knowledge, verify]. Useful when a runner-image update causes regressions. |

**Never collect:** `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, `ACTIONS_RUNTIME_TOKEN`, `GITHUB_TOKEN`, `ACTIONS_CACHE_URL`, or any `secrets.*`-derived variable. The adapter reads an **allowlist** of variable names. It never iterates over the whole environment (spec §10: "Never upload environment dumps").

### 1.2 Uniquely identifying a matrix job instance

Use these keys in order of preference:

1. **`check_run_id` from the verified OIDC token** [V-4]. The server trusts it because the token is signed. Cross-check it against the `FLEET_GITHUB_CHECK_RUN_ID` value the agent reports.
2. **`job.check_run_id` passed through the environment** [V-3], for static-token mode on github.com.
3. **Synthetic key** (GHES, or nothing passed): `(pipeline_key, GITHUB_JOB, strategy.job-index | sha256(canonical matrix JSON))`. This is stable across agent reconnects within one attempt, which satisfies spec §7 ("A job ID must remain stable during reconnection").
4. **Last resort:** a UUID generated by the agent and persisted in `$RUNNER_TEMP/.fleet/job-id`, plus an explicit `job.identity_quality = "weak"` flag shown in the UI [I].

Multiple `maestro-fleet run` invocations in the same job (spec §3: several suites or processes per worker) share the job key. Each gets its own `suite_execution_id`.

### 1.3 Building a link to the job log page

- **Run attempt page:** `{GITHUB_SERVER_URL}/{GITHUB_REPOSITORY}/actions/runs/{GITHUB_RUN_ID}/attempts/{GITHUB_RUN_ATTEMPT}` [I: standard GitHub URL shape].
- **Job page:** `{GITHUB_SERVER_URL}/{GITHUB_REPOSITORY}/actions/runs/{GITHUB_RUN_ID}/job/{check_run_id}`. The example `check_run_id` value `51725241954` in GitHub's docs is the number in the job URL path of the linked run [V-3]. In the REST API, a job's `id` and its check run share the same numeric ID [I: consistent with the Jobs API returning `check_run_url` ending in the same id; verify on GHES].
- **Options when `check_run_id` is not available (GHES):**
  - (a) The agent calls `GET {GITHUB_API_URL}/repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs` with `GITHUB_TOKEN` (needs `actions: read`), and matches on `runner_name == RUNNER_NAME` and `status == in_progress`. This is the agent calling the provider, which spec §12 allows ("must not require the *backend* to contact the CI provider"). It is ambiguous when several jobs share a runner name, and it costs API rate limit [I].
  - (b) Link to the run attempt page only.
  - (c) The user passes `FLEET_JOB_URL`.

  Make (a) opt-in (`--github-api-lookup`) and default to (b).
- `GITHUB_EVENT_PATH` does **not** contain the job ID. It is the webhook payload of the triggering event [V-1].

### 1.4 Other event-specific behaviour

- `pull_request` workflows **do not run when the PR has a merge conflict** [V-6]. Fleet will simply see no run, which is expected.
- `merge_group` (merge queue): `GITHUB_SHA` is the merge-group commit. Treat these runs as trusted, like pushes to protected branches [I].
- Re-running only failed jobs increments `GITHUB_RUN_ATTEMPT` for the run [I]. Jobs that are not re-run keep their previous `check_run_id` from attempt N−1. The UI should present "attempt 2" as a sparse overlay on attempt 1, not as a full new pipeline [I].
- Since 2025-12-08, `pull_request_target` always takes the workflow file and the checkout commit from the **default branch**, and environment protection rules evaluate against `GITHUB_REF` [V-8].

---

## 2. GitHub Actions OIDC for Fleet publishers

### 2.1 Requesting a token (agent side)

- The job needs `permissions: id-token: write`. That permission grants nothing else [V-2]. In reusable workflows outside the caller's org or enterprise, the **caller** must grant it [V-2].
- The runner exposes `ACTIONS_ID_TOKEN_REQUEST_URL` and `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. The agent does `GET "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=<aud>"` with `Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN` [V-2]. The response is JSON `{"value": "<jwt>"}` [I: toolkit behaviour].
- **Audience:** use a Fleet-specific value such as `https://fleet.example.com` or `maestro-fleet:<instance-id>`. The default `aud` is the repository owner URL [V-2], and Fleet must reject it so that tokens minted for other relying parties (AWS, Azure and so on) cannot be replayed against Fleet.
- **Issuer:**
  - github.com: `https://token.actions.githubusercontent.com` [V-2].
  - GHES: `https://HOSTNAME/_services/token` [V-9].
  - GitHub Enterprise Cloud custom issuers with an enterprise slug, and `*.ghe.com` data-residency issuers, exist [I: not re-verified]. Make the issuer list fully configurable.
- **Lifetime:** GitHub's OIDC reference does not state it [V-2]. Third parties report about 5 minutes [I-10]. Design rule: **exchange the token immediately** for a Fleet session credential, and request a fresh OIDC token only to re-authenticate after the session credential expires. `ACTIONS_ID_TOKEN_REQUEST_TOKEN` stays valid for the job, so the agent can mint again [I].

### 2.2 Claims available [V-2]

- Standard: `iss`, `aud`, `sub`, `exp`, `iat`, `nbf`, `jti`.
- Custom: `repository`, `repository_id`, `repository_owner`, `repository_owner_id`, `repository_visibility`, `actor`, `actor_id`, `workflow`, `workflow_ref`, `workflow_sha`, `job_workflow_ref`, `job_workflow_sha`, `run_id`, `run_number`, `run_attempt`, `check_run_id` (added 2025-11-13 [V-4]), `ref`, `ref_type`, `head_ref`, `base_ref`, `event_name`, `environment`, `runner_environment`, and `repo_property_*` (org custom properties).
- GHES tokens add `enterprise` and `enterprise_id` [V-9]. `ref_protected` did not appear on the current github.com reference page [V-2], so do not depend on it.
- **There is no "is fork" claim.** A `pull_request` token from a same-repo branch and one from a fork (if a private repo enables fork write tokens) look identical apart from `head_ref` [I]. The policy must therefore not assume "PR token implies trusted".

### 2.3 Subject customization and the 2026 immutable-subject change

- The default `sub` forms are `repo:ORG/REPO:environment:NAME`, `repo:ORG/REPO:pull_request` and `repo:ORG/REPO:ref:refs/heads/BRANCH` [V-2].
- Repo-level customization: `PUT /repos/{owner}/{repo}/actions/oidc/customization/sub` with `include_claim_keys` and `use_default` [V-2]. Org-level templates also exist, and repos must opt in [V-2].
- **New in 2026:** immutable subjects `repo:octocat@123456/my-repo@456789:ref:refs/heads/main` apply automatically to repos created, renamed or transferred after **2026-07-15**. Existing repos can opt in. github.com only, not GHES [V-5].
- **Fleet implication:** never match on `sub`. Match on `repository_id`, `repository_owner_id` and the other individual claims. Subject customization then becomes irrelevant to Fleet, which removes a whole class of IT misconfiguration.

### 2.4 Pull requests from forks

- For `pull_request` events from forks, secrets other than `GITHUB_TOKEN` are not passed, and `GITHUB_TOKEN` is read-only [V-6]. Write permissions are "adjusted to read only" unless an admin enables **"Send write tokens to workflows from pull requests"** (private repos) [V-7].
- `id-token` has only `write` and `none` levels [V-7], so on fork PRs no OIDC token can be minted [I: follows from the downgrade; third-party docs confirm, I-11].
- `pull_request_target` runs in the base repo's context with read/write `GITHUB_TOKEN` even from public forks [V-7]. GitHub warns "Avoid using this event if you need to build or run code from the pull request" [V-6]. Workflows using `pull_request_target` or `workflow_run` "must not explicitly check out untrusted code" [V-12]. Building the app and running Maestro **is** running PR code, so `pull_request_target` must never be used for Fleet-wrapped jobs.
- **Self-hosted runners** (Mac minis) "should almost never be used for public repositories" [V-12]. A malicious PR can persist on a non-ephemeral runner and steal later jobs' tokens.

### 2.5 Server-side verification (recommended algorithm) [I]

1. **Issuer allowlist.** Only configured issuers are accepted. Never resolve JWKS from an unconfigured `iss` (prevents SSRF and key confusion).
2. **JWKS.** Fetch through the issuer's `/.well-known/openid-configuration` [V-2] at startup. Cache it with a TTL of about 1 h. On an unknown `kid`, refetch at most once per minute. Use `jsonwebtoken` plus a small JWKS cache. Hand-rolling JWT parsing is not acceptable.
3. **Algorithm pinning.** Accept RS256 only. Reject `none`, HS*, and any `alg` that does not match the key's `kty`.
4. **Standard checks.** `aud` must equal the configured Fleet audience. Check `exp` and `nbf` with 60 s leeway. Reject `iat` older than 10 min. Store `jti` in a replay table (Postgres, TTL equal to `exp`) and accept each `jti` once.
5. **Project binding.** Find publisher rules where `issuer`, `repository_id` and `repository_owner_id` match. Optional narrowing: `job_workflow_ref` (forces a central reusable workflow), `runner_environment`, `environment`, `repo_property_*`.
6. **Trust level.** Derive it from `event_name` and `ref` (§2.6).
7. **Exchange.** Issue a **Fleet session credential**: 256-bit random, stored hashed. It is bound to `{project_id, pipeline_key, check_run_id, trust_level, capabilities, quota}`, with TTL equal to the configurable max job length (default 6 h, the GitHub-hosted job limit [I]) and refreshable while heartbeats continue. Capabilities: `telemetry:write`, `artifact:upload(session-scoped)`, `session:finalize`. **No read capabilities**, as spec §10 requires.
8. **Consistency checks.** Server-derived fields (run_id, attempt, check_run_id, repository) **override** agent-reported ones. Mismatches are logged as a security event.

Example publisher rule (server config, TOML) [I]:

```toml
[[projects."mobile-app".publishers]]
provider            = "github-actions"
issuer              = "https://token.actions.githubusercontent.com"
audience            = "https://fleet.example.com"
repository_id       = 123456789
repository_owner_id = 9876543
# optional: only accept tokens minted inside the org's blessed reusable workflow
# job_workflow_ref  = "acme/ci-templates/.github/workflows/maestro.yml@refs/heads/main"
trusted_refs        = ["refs/heads/main", "refs/heads/release/*", "refs/tags/v*"]
allowed_events      = ["push", "schedule", "workflow_dispatch", "merge_group", "pull_request"]
pr_policy           = "restricted"       # "restricted" | "full" | "deny"
max_session_bytes   = "2GiB"
```

### 2.6 Trust levels and the untrusted-PR policy (recommendation) [I]

| Trust level | Granted when | Allowed | Analytics |
|---|---|---|---|
| `trusted` | `push`, `schedule`, `workflow_dispatch` or `merge_group` on a `trusted_refs` ref | Full telemetry and the full artifact policy | Counts toward baselines, flakiness and trends |
| `pr` | `pull_request` (same-repo or allowed by settings) | Full telemetry. Artifacts per policy; HTML reports are **never rendered inline**, download only | Shown on the PR. Excluded from default-branch baselines, so a PR cannot poison flakiness history |
| `untrusted` | Bundle published by a `workflow_run` relay from a fork PR (opt-in) | JUnit, normalized events and text logs only (redacted again server-side). **No screenshots, videos or HTML.** Strict quota. 7-day retention | Excluded. Labelled "unverified provenance" |
| *(rejected)* | `pull_request_target`, unknown events, refs not allowed | — | — |

**Default for fork PRs: no publishing.** The agent detects that `ACTIONS_ID_TOKEN_REQUEST_URL` is absent or `pull_request.head.repo.fork == true`. It then runs in **offline mode**: Maestro runs unchanged, the exit code is preserved, and the agent writes `$RUNNER_TEMP/fleet/bundle/` with normalized events, a manifest and SHA-256 sums. It prints one line, "Fleet: publishing disabled for fork PR (no OIDC token)", and exits with Maestro's code. This meets spec §10 ("Untrusted pull-request code must not receive privileged publishing credentials…") and spec §13 (backend unavailable → preserve CI behaviour).

**Optional relay (opt-in):** the fork job uploads the bundle with `actions/upload-artifact`. A separate `on: workflow_run` workflow in the base repo, which has `id-token: write` and **does not check out PR code**, downloads the artifact and runs `maestro-fleet publish-bundle --provenance untrusted`. The server treats every byte as hostile, for example re-parsing JUnit with a hardened parser. GitHub recommends `workflow_run` for privilege separation and says to treat its artifacts "with caution" [V-12]. Live progress is impossible in this mode; only the post-hoc result is published.

---

## 3. Other CI providers and the adapter trait

### 3.1 Provider notes

**GitLab CI**

- Variables [V-18]:
  - Pipeline and job: `CI_PIPELINE_ID` (instance-unique), `CI_PIPELINE_IID` (per-project number), `CI_PIPELINE_URL`, `CI_JOB_ID` (instance-unique, so this is the job-instance key), `CI_JOB_NAME`, `CI_JOB_URL`, `CI_JOB_STARTED_AT`.
  - Parallel jobs: `CI_NODE_INDEX` and `CI_NODE_TOTAL` (`parallel:`).
  - Repository and commit: `CI_PROJECT_ID`, `CI_PROJECT_PATH`, `CI_COMMIT_SHA`, `CI_COMMIT_REF_NAME`, `CI_COMMIT_BRANCH`.
  - Merge requests: `CI_MERGE_REQUEST_IID`, `CI_MERGE_REQUEST_SOURCE_BRANCH_SHA` (merged-results pipelines), `CI_MERGE_REQUEST_DIFF_BASE_SHA`, `CI_MERGE_REQUEST_SOURCE_PROJECT_PATH` (which may be a fork).
  - Trigger and runner: `CI_PIPELINE_SOURCE`, `CI_SERVER_URL`, `CI_RUNNER_ID`, `CI_RUNNER_DESCRIPTION`, `CI_RUNNER_TAGS`.
  - **`CI_JOB_RETRY_COUNT`** (GitLab 19.3+) gives the attempt number.
- OIDC: `id_tokens: { FLEET_ID_TOKEN: { aud: https://fleet.example.com } }` [V-19].
  - The issuer is the GitLab instance URL [V-19].
  - Lifetime equals the job timeout, or 5 min if none is set [V-19].
  - Claims include `project_id`, `namespace_id`, `pipeline_id`, `pipeline_source`, `job_id`, `ref`, `ref_protected`, `sha`, `runner_environment` and `ci_config_ref_uri` [V-19].
  - In MR pipelines the `project_*` and `ref` claims describe the **source** project. `source_project_*` claims were added in GitLab 19.5 [V-19]. Bind Fleet projects to `project_id` and treat fork-sourced MRs (`source_project_id != project_id`) as `untrusted`.
  - GitLab gives `ref_protected`, which maps cleanly onto `trusted`.

**Buildkite**

- `buildkite-agent oidc request-token --audience <aud> [--lifetime <s>] [--claim ...]` [V-21].
- Issuer `https://agent.buildkite.com`. Default lifetime 5 min [V-21].
- Default `sub` is `organization:ORG:pipeline:PIPE:ref:REF:commit:SHA:step:KEY`. Claims include `organization_id`, `pipeline_id`, `build_number`, `build_branch`, `build_commit`, `build_source`, `job_id`, `agent_id`, `step_key` and `runner_environment` [V-21].
- Bind on `organization_id` and `pipeline_id` (UUIDs).
- Env vars such as `BUILDKITE_BUILD_ID`, `BUILDKITE_BUILD_NUMBER`, `BUILDKITE_JOB_ID`, `BUILDKITE_STEP_KEY`, `BUILDKITE_PARALLEL_JOB`, `BUILDKITE_RETRY_COUNT`, `BUILDKITE_BUILD_URL`, `BUILDKITE_COMMIT`, `BUILDKITE_BRANCH`, `BUILDKITE_PULL_REQUEST`, `BUILDKITE_PULL_REQUEST_REPO` and `BUILDKITE_AGENT_NAME` [I: prior knowledge; verify against the Buildkite env reference].

**CircleCI**

- Tokens: `$CIRCLE_OIDC_TOKEN`, `$CIRCLE_OIDC_TOKEN_V2`, and `$CIRCLE_OIDC_TOKEN_V3` (Cloud only, recommended) [V-22].
- Issuer `https://oidc.circleci.com/org/<org-id>`. Default `aud` is the org ID. Lifetime is 1 h [V-22].
- Key claim: `oidc.circleci.com/project-id` [V-22].
- **Fork builds get tokens only if "Pass secrets to builds from forked pull requests" is enabled**, in which case policies must check `vcs-origin` or `checkout-source` [V-22]. Fleet should require `checkout-source` to equal the bound repository, or else downgrade to `untrusted`.
- Custom audiences: see "OIDC tokens with custom claims" [V-22]. The `circleci run oidc get` command was not found on that page and is unverified.

**Jenkins**

- No native workload identity. The **OpenID Connect Provider plugin** (`oidc-provider`) makes Jenkins an issuer at `https://JENKINS/oidc` [V-26].
- Tokens are injected with `withCredentials` as an "OpenID Connect id token" credential (secret text or file) [V-26].
- `sub` defaults to the job URL, and claims are templatable, for example `${BRANCH_NAME}` [V-26].
- Lifetime is not documented [V-26].
- Jenkins must serve its JWKS anonymously [V-26], so Fleet must reach it. That conflicts with "no Fleet→CI contact" only in spirit, because it is an identity fetch, not a CI fetch. An alternate static issuer is supported [V-26].
- Fallback: a scoped static `FLEET_TOKEN` (spec §10).
- Env vars `BUILD_NUMBER`, `BUILD_URL`, `JOB_NAME`, `NODE_NAME`, `EXECUTOR_NUMBER`, `GIT_COMMIT`, `BRANCH_NAME`, `CHANGE_ID` and `CHANGE_TARGET` [I]. Jenkins has no attempt concept: a rebuild is a new build number.

### 3.2 Rust adapter design [I]

```rust
/// Immutable snapshot of only the env vars an adapter declared it reads.
pub struct EnvSnapshot { /* BTreeMap<&'static str, String> built from allowlists */ }

#[derive(Clone, Debug, Serialize)]
pub struct CiContext {
    pub provider: ProviderKind,            // GithubActions | GitlabCi | Buildkite | CircleCi | Jenkins | Generic
    pub server_url: Option<Url>,
    pub repository: RepoRef,               // display name + immutable provider id
    pub pipeline: PipelineRef,             // provider_run_id, attempt, number, name, url, workflow_ref
    pub job: JobRef,                       // key, provider_job_id, matrix (json), index/total, name, url,
                                           // identity_quality: Strong | Synthetic | Weak
    pub commit: CommitRef,                 // tested_sha, head_sha, base_sha
    pub change: Option<ChangeRef>,         // pr/mr number, head/base branch, is_fork
    pub trigger: Option<String>,           // event_name / pipeline_source / build_source
    pub worker: WorkerRef,                 // name, os, arch, kind (hosted/self-hosted), image
}

pub enum Credential {
    WorkloadIdentity { jwt: SecretString, issuer_hint: Option<String> },
    StaticToken(SecretString),
    None { reason: NoCredentialReason },   // e.g. ForkPullRequest, NotConfigured
}

#[async_trait::async_trait]
pub trait CiProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    /// Env var names this adapter may read (also used to build the redaction set's *exclusion* list).
    fn env_allowlist(&self) -> &'static [&'static str];
    /// Env var names whose *values* are secrets and must be added to the redaction set.
    fn secret_env_names(&self) -> &'static [&'static str];
    fn detect(env: &EnvSnapshot) -> bool where Self: Sized;
    fn context(&self, env: &EnvSnapshot, event: Option<&EventPayloadView>) -> Result<CiContext, AdapterError>;
    /// Mint a workload-identity token for `audience`, if the platform supports it here.
    async fn credential(&self, audience: &str, http: &HttpClient) -> Result<Credential, AdapterError>;
    /// Best-effort "is this untrusted code?" hint (fork PR etc.). Server decides authoritatively.
    fn untrusted_hint(&self, ctx: &CiContext) -> bool;
}
```

- **Detection order:** explicit `FLEET_CI_PROVIDER`, then `GITHUB_ACTIONS=true`, then `GITLAB_CI=true`, then `BUILDKITE=true`, then `CIRCLECI=true`, then `JENKINS_URL`+`BUILD_NUMBER`, then Generic.
- **Generic fallback and overrides:** every field can be set or overridden with `FLEET_*` variables. These are applied *after* adapter detection, so users can patch gaps such as a GHES job URL:
  - `FLEET_SERVER_URL`, `FLEET_PROJECT`, `FLEET_TOKEN` / `FLEET_TOKEN_FILE`, `FLEET_OIDC_TOKEN_FILE`
  - `FLEET_PIPELINE_ID`, `FLEET_PIPELINE_ATTEMPT`, `FLEET_PIPELINE_NUMBER`, `FLEET_PIPELINE_NAME`, `FLEET_PIPELINE_URL`
  - `FLEET_JOB_KEY`, `FLEET_JOB_ID`, `FLEET_JOB_NAME`, `FLEET_JOB_URL`, `FLEET_JOB_INSTANCE`, `FLEET_CI_MATRIX`
  - `FLEET_REPOSITORY`, `FLEET_REPOSITORY_ID`, `FLEET_COMMIT_SHA`, `FLEET_HEAD_SHA`, `FLEET_BASE_SHA`, `FLEET_BRANCH`, `FLEET_BASE_BRANCH`, `FLEET_PR_NUMBER`, `FLEET_EVENT`
  - `FLEET_WORKER_NAME`, `FLEET_WORKER_KIND`
- **Overrides never override server-verified claims.** When an OIDC claim exists, the server ignores the corresponding `FLEET_*` value and flags the mismatch.
- `maestro-fleet inspect` prints the resolved `CiContext` (with secrets elided) and where each field came from: `env:GITHUB_RUN_ID`, `override:FLEET_JOB_URL`, `default`, and so on. This is the main debugging tool for adapter problems.

---

## 4. Example GitHub Actions workflow (Maestro wrapped by maestro-fleet, OIDC)

```yaml
name: android-e2e
on:
  push:
    branches: [main, "release/**"]
  pull_request:          # same-repo PRs publish with trust=pr; fork PRs run offline (no token)
  workflow_dispatch:

permissions:
  contents: read         # least privilege at workflow level

concurrency:
  group: e2e-${{ github.ref }}
  cancel-in-progress: true   # Fleet must record these as cancelled, not failed (spec §13)

jobs:
  maestro:
    name: Maestro ${{ matrix.suite }} (API ${{ matrix.api-level }})
    runs-on: ubuntu-24.04          # x64 Linux: KVM available; macOS arm64 cannot run the emulator
    timeout-minutes: 60
    permissions:
      contents: read
      id-token: write              # lets the agent mint an OIDC token for Fleet
    strategy:
      fail-fast: false
      matrix:
        api-level: [34, 35]
        suite: [smoke, kyc]
    env:
      FLEET_SERVER_URL: https://fleet.example.com
      FLEET_PROJECT: mobile-app
      FLEET_OIDC_AUDIENCE: https://fleet.example.com
      # Not default env vars: pass them explicitly.
      FLEET_GITHUB_CHECK_RUN_ID: ${{ job.check_run_id }}
      FLEET_JOB_INSTANCE: ${{ strategy.job-index }}
      FLEET_CI_MATRIX: ${{ toJSON(matrix) }}
    steps:
      - uses: actions/checkout@<full-commit-sha>        # v7.x — pin by SHA
      - uses: actions/setup-java@<full-commit-sha>      # Maestro needs a JDK
        with: { distribution: temurin, java-version: "17" }

      - name: Enable KVM
        run: |
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \
            | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules
          sudo udevadm trigger --name-match=kvm

      - name: Install Maestro (pinned)
        run: |
          export MAESTRO_VERSION=2.0.0        # pin; example version
          curl -fsSL "https://get.maestro.mobile.dev" | bash
          echo "$HOME/.maestro/bin" >> "$GITHUB_PATH"

      - name: Install maestro-fleet (pinned + verified)
        uses: acme/setup-maestro-fleet@<full-commit-sha> # v1
        with:
          version: 1.4.2                       # checksum for each version is embedded in the action
          verify-attestation: true             # optional: gh attestation verify

      - name: AVD cache
        uses: actions/cache@<full-commit-sha>   # v5
        id: avd-cache
        with:
          path: |
            ~/.android/avd/*
            ~/.android/adb*
          key: avd-${{ matrix.api-level }}-x86_64-google_apis

      - name: Create AVD snapshot (cache miss only)
        if: steps.avd-cache.outputs.cache-hit != 'true'
        uses: reactivecircus/android-emulator-runner@<full-commit-sha>   # v2.38.0
        with:
          api-level: ${{ matrix.api-level }}
          arch: x86_64
          target: google_apis
          force-avd-creation: false
          emulator-options: -no-window -gpu swiftshader_indirect -noaudio -no-boot-anim -camera-back none
          disable-animations: false
          script: echo "Generated AVD snapshot"

      - name: Run Maestro under Fleet
        uses: reactivecircus/android-emulator-runner@<full-commit-sha>   # v2.38.0
        env:
          # Secrets reach Maestro via MAESTRO_* env, NOT via `-e KEY=VALUE` on argv.
          MAESTRO_TEST_PASSWORD: ${{ secrets.MAESTRO_TEST_PASSWORD }}
        with:
          api-level: ${{ matrix.api-level }}
          arch: x86_64
          target: google_apis
          force-avd-creation: false
          emulator-options: -no-window -gpu swiftshader_indirect -no-snapshot-save -noaudio -no-boot-anim -camera-back none
          disable-animations: true
          # The action runs each script line as a separate shell command: keep it on ONE line.
          script: >-
            maestro-fleet run --suite ${{ matrix.suite }} --offline-bundle "$RUNNER_TEMP/fleet/bundle"
            -- maestro test --format junit --output build/fleet/report.xml
            --test-output-dir build/fleet/artifacts --debug-output build/fleet/artifacts
            .maestro/${{ matrix.suite }}/

      # Fork PRs: no OIDC token -> agent ran offline; keep the bundle for an optional trusted relay.
      - name: Upload offline Fleet bundle
        if: always() && github.event.pull_request.head.repo.fork == true
        uses: actions/upload-artifact@<full-commit-sha>
        with:
          name: fleet-bundle-${{ matrix.suite }}-${{ matrix.api-level }}
          path: ${{ runner.temp }}/fleet/bundle
          retention-days: 3
```

Notes on the example:

- The KVM udev snippet, the default emulator options, the cache paths and v2.38.0 (released 5 July) come from the action's README and releases page [V-15]. `actions/checkout` v7.0.1 is the latest release [V-27].
- `-no-snapshot-save` uses the cached snapshot without writing it back [V-15, V-16].
- Maestro's `--shard-split` and `--shard-all` shard across *devices attached to one host*, not across CI jobs [I]. To split across jobs, partition by directory or tag, as the matrix above does.
- The step-level `env:` pattern also avoids script injection. GitHub recommends mapping untrusted expressions to environment variables [V-12].

---

## 5. Running Android emulators in CI

### 5.1 Runner options (state as of 2026-11)

| Runner | Emulator viable? | Notes |
|---|---|---|
| GitHub-hosted **Linux x64** (`ubuntu-latest`, `ubuntu-24.04`) | **Yes (recommended)** | "GitHub-hosted Linux runners support hardware acceleration for Android SDK tools" [V-14]. Apply the udev rule to make `/dev/kvm` accessible [V-15]. Sizes: 4 vCPU/16 GB (public repos) or **2 vCPU/8 GB (private repos)** [V-14]. On private repos plan for one emulator per job, or use larger runners. |
| GitHub-hosted **Linux arm64** (`ubuntu-24.04-arm`) | Not with KVM | Third-party reports say `/dev/kvm` is not exposed [I-28]. Redroid (containerized Android) is an alternative, but it is not the stock emulator. |
| GitHub **larger runners** (Linux) | Yes | The README positions larger Linux runners as the preferred path [V-15]. More vCPUs allow 2–4 emulators per job [I]. |
| GitHub-hosted **macOS arm64** (`macos-latest`, M1, 3 vCPU/7 GB) | **No** | "Nested-virtualization is not supported due to the limitation of Apple's Virtualization Framework" [V-14]. Confirmed in runner-images issue #9741 [V-13]. |
| GitHub-hosted **macOS Intel** (`macos-15-intel`, `macos-26-intel`, 4 vCPU/14 GB) | Historically yes | Still listed [V-14]. x86_64 macOS is reportedly supported only until fall 2027 [I-17]. Not a long-term plan. |
| **Self-hosted Apple Silicon Mac minis** | Yes | Use **arm64-v8a** system images. Acceleration needs a host-matching image and uses HVF on macOS [V-16]. Lets iOS simulators and Android emulators share hardware. Must not serve fork PRs [V-12]. |
| Self-hosted Linux x64 bare metal or VMs with nested virtualization | Yes | KVM. The best density per dollar [I]. |

### 5.2 Emulator flags and caching

- **CI flags:** `-no-window` (headless) [V-16]; `-noaudio`/`-no-audio` [V-16]; `-no-boot-anim` [V-16]; `-gpu swiftshader_indirect` (software GPU, the action's default) [V-15, V-16]; `-no-snapshot` or `-no-snapshot-save` [V-16]; `-memory <1536–8192 MB>` [V-16]; `-accel on` (fail fast instead of falling back to very slow TCG; `off` additionally needs `ANDROID_I_WANT_MY_TCG=yes`) [V-16]; `-camera-back none`; `-netfast` [V-16]; `-port <even 5554..5682>` [V-16].
- **Multiple emulators on one host:** each instance takes a console and adb port pair starting at 5554/5555. The range is 5554–5682, allowing **64 instances** [V-16]. Use even console ports [V-16]. To run one AVD several times, use `-read-only` or clone the AVDs [I].
- **Resources per emulator** [I]: about 2 vCPU (more with swiftshader, because GPU rendering then happens on the CPU), 2–4 GB RAM (the `-memory` value plus about 1 GB of host overhead), and 6–10 GB disk per AVD including the snapshot. A 32 GB, 10-core Apple Silicon Mac mini realistically runs 3–4 parallel emulators. Fleet's worker view should show **per-emulator RSS and CPU** so teams can find that ceiling empirically, which is one of the product's value propositions.
- **Snapshot caching:** cache `~/.android/avd/*` and `~/.android/adb*`, generate the snapshot on a cache miss, then run with `-no-snapshot-save` [V-15].
- **Tooling change:** the standalone `emulator` command is now marked **deprecated** in favour of the new **Android CLI** (`android emulator create|start|stop|list`) [V-16]. The agent should treat the emulator as opaque and observe processes rather than invoking it. Fleet does not start emulators (spec §17 non-goals).

### 5.3 Implications for agent metrics [I unless noted]

- **Process discovery:** match names `qemu-system-x86_64`, `qemu-system-x86_64-headless` [V-29], `qemu-system-aarch64` and `qemu-system-aarch64-headless`, plus the parent `emulator` and helpers such as `netsimd` and `crashpad_handler`. In headless mode the binary is the `-headless` variant [V-29]. Use a prefix match on `qemu-system-`.
- **Device↔process mapping:** parse only `-port`/`-ports` and `-avd`/`@name` from the qemu or emulator cmdline. Device serial = `emulator-<console port>`. Confirm with `adb -s <serial> emu avd name`. **Never forward the raw cmdline** (spec §10). Emulator cmdlines can contain `-http-proxy http://user:pass@...` [V-16].
- **Acceleration status** belongs in `selftest` and as a device attribute: check that `/dev/kvm` exists and is read/write, or run `emulator -accel-check` [V-16]. When acceleration is missing, report `acceleration: none` loudly, because it explains 5–10× slowdowns.
- **Thermals:** GitHub-hosted VMs expose no meaningful sensors. On macOS, `powermetrics` needs root. Report `unavailable`, as spec §16 item 5 requires.
- **Containers:** with emulators in docker-android or Redroid, qemu may live in a different PID namespace. Report emulator metrics as `unavailable (not visible from agent)`, not as zero.
- **Ephemeral runners:** a worker identity exists per job on hosted runners. Self-hosted Mac minis give long-lived workers whose history spans jobs, which is valuable for detecting a "flaky machine".

---

## 6. Security model hardening

### 6.1 Threat model

Assets:

- **A1** publisher credentials (OIDC tokens, Fleet session tokens, static tokens)
- **A2** SSO sessions
- **A3** artifacts (screenshots, videos, logs, reports)
- **A4** secrets that appear in test inputs
- **A5** the DB and object store, plus their credentials
- **A6** Fleet signing keys and config
- **A7** CI runners, especially self-hosted ones

Actors:

- **E1** external fork contributor
- **E2** insider with write access, or a compromised dependency in the app or test repo
- **E3** compromised runner
- **E4** authenticated low-privilege dashboard user
- **E5** network attacker
- **E6** compromised Fleet supply chain

| # | Threat | Actor → Asset | Mitigations |
|---|---|---|---|
| T1 | Malicious fork PR exfiltrates publisher credentials | E1 → A1 | OIDC only, with no long-lived secret in the repo. Fork PRs get no `id-token` or secrets [V-6, V-7]. Offline mode by default. Forbid `pull_request_target` for Fleet jobs [V-6, V-12]. The static `FLEET_TOKEN` fallback is documented as push/schedule only, stored as an **environment** secret with protection rules. |
| T2 | Insider or same-repo PR mints a token and forges results or poisons history | E2 → A3/A5 | Token scope is write-only for one session, bound to `check_run_id`. The `pr` trust level is excluded from baselines. Server-verified claims override agent-reported metadata. Results are observability, **not** a quality gate: the CI exit code stays authoritative (spec §16 item 11). Per-session quotas. |
| T3 | A stolen static token is reused elsewhere | E1/E2 → A1 | Prefer OIDC. Static tokens are per project, hashed at rest, with an expiry (≤ 90 days by default) and rotation audited (spec §10 Audit). They can only *open sessions*. Optional IP or CIDR allowlist per token. Exchange for short-lived session tokens. |
| T4 | OIDC replay or audience confusion | E2/E5 → A1 | Fleet-specific `aud`. `jti` single-use. `iat` max age. Issuer allowlist. RS256 only. JWKS only from configured issuers (§2.5). |
| T5 | **Credential-bearing argv**, e.g. `maestro test -e PASSWORD=...` [V-20] | E2 / accident → A4 | The agent stores argv as a *structured, redacted* form: flags kept, values of `-e`/`--env`/`--env=` replaced with `KEY=[REDACTED]`, and unknown flag values redacted by default. **Recommend `MAESTRO_*` env vars over `-e`**, because argv is visible in `ps` and in CI logs. All `-e` values and all `MAESTRO_*` env values [V-20] go into the **known-secret set** (§6.3). |
| T6 | Secrets inside Maestro output: console, `maestro.log`, commands JSON with resolved `inputText`, HTML report | E2 / accident → A4 | Exact-match redaction of the known-secret set (plain, URL-encoded, base64 and JSON-escaped forms) on every streamed line **and** on text artifacts before upload. Then gitleaks-style regex rules. The server repeats regex redaction on ingest as defense in depth. Console streaming is capped (spec §10 "Limit console log collection"). |
| T7 | PII or secrets visible in screenshots and videos | E2/E4 → A3 | Treat media as sensitive class. No pixel redaction (unreliable). Per-project policy: `screenshots: failures-only` by default and `video: off` by default. Short retention. Access only via authorized, expiring links. Optional audit of every media access (spec §10 Audit). Encourage tests to use synthetic accounts. |
| T8 | Stored XSS via flow names, test names, tags, branch names, step text, log lines | E2 → A2 | React's default escaping. Ban `dangerouslySetInnerHTML` (lint rule). Render logs as text nodes. Convert ANSI SGR codes to an allowlisted span model. Strict CSP on the app origin: `default-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`. No Markdown or HTML rendering of user strings. |
| T9 | Malicious HTML report runs script in the Fleet origin | E2 → A2 | **Never serve user HTML from the app origin.** Use a separate *registrable* domain such as `fleet-usercontent.example.net` (not merely a subdomain; §6.2). |
| T10 | Content sniffing: an "image" is actually HTML or SVG | E2 → A2 | Validate magic bytes against an allowlist (PNG, JPEG, WebP, MP4, WebM, XML/JUnit, text, JSON). **No SVG.** Server-side content-type is set from validation, never from the client. `X-Content-Type-Options: nosniff`. Non-media types are served as `Content-Disposition: attachment`. |
| T11 | Zip bomb, path traversal or symlinks in artifact bundles | E2/E1 → A5 | Prefer **per-file uploads** with a manifest (size, SHA-256, type) over archives. If archives are accepted: cap entry count (for example 5,000), total uncompressed bytes, per-entry ratio (for example 100:1) and nesting depth (0). Reject absolute paths, `..`, NUL, symlinks, hardlinks and device files. Normalize Unicode. Use the `zip` crate's `enclosed_name()`. Count bytes while streaming and never trust headers. Unpack only in a temp dir with quotas. |
| T12 | Upload DoS or storage exhaustion | E2/E3 → A5 | Per-session byte and file quotas (default 2 GiB and 2,000 files). Per-project daily quota. Max WS frame (for example 256 KiB) and message rate. Abort incomplete multipart uploads with a lifecycle rule plus a server sweeper. A server-side retention job. |
| T13 | Log injection: ANSI/OSC escapes (OSC 8 links, OSC 52 clipboard), CR overwrite, fake lines, CRLF in Fleet's own logs | E2 → A2 | Strip all non-SGR escape sequences on ingest. Normalize CR. Mark agent-synthesized lines distinctly from Maestro output. Fleet's own logs are structured JSON (`tracing` + JSON), so values are escaped. The agent never echoes server-supplied strings to CI stdout without stripping leading `::` (GitHub workflow commands). |
| T14 | SSRF | E2/E4 → A5 | The server never fetches URLs supplied by agents or users. Report links are displayed, not fetched. JWKS and IdP discovery only from admin-configured issuers. Any future webhooks: deny RFC1918, link-local, metadata IPs and loopback after DNS resolution, plus an egress allowlist. |
| T15 | Presigned or download-link leakage (Referer, chat paste) | E4 → A3 | TTL ≤ 5 min. Links are minted per request after an authorization check. `Referrer-Policy: no-referrer`. Bound to the artifact ID and, optionally, the user ID (Fleet-signed links). Prefer **Fleet-proxied streaming** with a short-lived capability token, so object storage never needs to be reachable from browsers or runners. |
| T16 | IDOR across projects | E4 → A3 | Every query scoped by `project_id` from the session's authorizations. UUIDv7 or random IDs (not a defense on their own). Integration tests for every endpoint × role × project (spec §15 security testing). |
| T17 | Cross-site WebSocket hijacking of browser live updates | E5/E4 → A3 | Check the `Origin` header on WS upgrade against the configured app origin. Use a SameSite=Lax host-only cookie or a short-lived WS ticket issued over REST. Per-project subscription authorization. |
| T18 | Compromised agent binary or action | E6 → A7/A1 | Pinned versions plus embedded SHA-256 checksums. GitHub artifact attestations with Sigstore. Immutable releases. Action pinned by SHA (§9). |
| T19 | Persistent compromise of a self-hosted runner | E1/E2 → A7 | No fork PRs on self-hosted runners [V-12]. Ephemeral or JIT runners where possible. A separate runner group for Fleet-instrumented jobs. The agent runs unprivileged and has no daemon (spec §2). |
| T20 | Exposed DB or object store credentials, or a public bucket | E5 → A5 | The object store and DB sit only on the internal Compose network. The bucket has no anonymous access. Fleet uses one key scoped to its bucket. Secrets come through Docker secrets or `*_FILE` env. TLS at the proxy. |
| T21 | IdP group-claim mishandling (Entra overage leads to no groups, then a wrong role) | E4 → A2/A3 | Default-deny when groups are missing. Detect overage via `_claim_names`/`hasgroups` [V-38]. Prefer app roles. Re-evaluate on every login with a bounded session lifetime (§8). |

### 6.2 Serving user-uploaded HTML safely [I, standard web-security practice]

- **Separate site.** Use a different registrable domain from the app (`*.example.net` vs `fleet.example.com`), so cookies, `document.domain` tricks and same-site requests cannot reach the app. App session cookies are host-only (no `Domain=`), `Secure`, `HttpOnly`, `SameSite=Lax`.
- **Headers on every user-content response:**

  ```http
  Content-Security-Policy: sandbox; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self' data:; form-action 'none'; frame-ancestors https://fleet.example.com
  X-Content-Type-Options: nosniff
  Cross-Origin-Resource-Policy: same-site
  Cross-Origin-Opener-Policy: same-origin
  Referrer-Policy: no-referrer
  Cache-Control: private, max-age=300
  ```

  `CSP: sandbox` without `allow-scripts` and without `allow-same-origin` gives the document an opaque origin with scripts disabled.
- **Authorization without cookies.** The user-content origin carries no session. Access uses a capability path such as `/u/<artifact-id>/<HMAC(artifact, user, exp)>/<file>` with a 5-minute expiry, minted by the app after an authorization check.
- **Embedding.** If the dashboard embeds a report, use `<iframe sandbox src="https://fleet-usercontent…">` with an empty sandbox attribute. The primary UX should render JUnit natively in Fleet and offer the Maestro HTML report as a download.
- **Single-domain fallback.** If IT can provide only one hostname, serve HTML reports only as `Content-Disposition: attachment` and never inline. Document this as a reduced-functionality mode.

### 6.3 Redaction design [I]

1. **Known-secret set**, built at agent start:
   - values of `-e`/`--env` arguments
   - values of `MAESTRO_*` env vars [V-20]
   - values of env vars whose **names** match `(?i)(pass(word|wd)?|secret|token|api[_-]?key|private[_-]?key|credential|auth|session|cookie)`
   - `FLEET_TOKEN`
   - the OIDC JWT once minted
   - values passed with `--redact-value` or `FLEET_REDACT_ENV=NAME1,NAME2`

   Values shorter than 6 characters are skipped to avoid destroying logs. For each value, also add its URL-encoded, base64, JSON-escaped and shell-quoted variants. Match with **Aho-Corasick** (`aho-corasick` crate) in O(n) per line.
2. **Pattern rules (gitleaks-style).** Embed a curated subset of gitleaks' MIT-licensed rule set [I: license, verify], compiled into a `regex::RegexSet`. Examples: AWS access keys `AKIA[0-9A-Z]{16}`; GitHub tokens `gh[pousr]_[A-Za-z0-9]{36,}` and `github_pat_…`; Slack `xox[abprs]-…`; Google API keys `AIza[0-9A-Za-z\-_]{35}`; JWTs `eyJ[\w-]+\.[\w-]+\.[\w-]+`; PEM private-key blocks; `Authorization: Bearer …`; URL userinfo `://user:pass@`. Keep the entropy-based generic rule **off** by default, because it is noisy on test logs.
3. **Where redaction applies.** Streamed console lines, step text, failure messages, JUnit `<failure>` and `<system-out>`, and the text and JSON debug artifacts listed in the allowlist. Binary media is never redacted, only access-controlled (T7).
4. **Server second pass.** Run the same regex set on ingest, configurable per project. The server never sees the known-secret values (they stay on the runner).
5. **Tests.** A golden corpus of secrets in every encoding, run in CI (spec §15 "secret redaction").

---

## 7. Self-hosted object storage (late 2026)

### 7.1 MinIO: what happened (dated facts)

| Date | Event | Source |
|---|---|---|
| 2025-02-26 → May 2025 | The community console was reduced to an "object browser only". Account, policy, OIDC and config management were removed from the web UI. Users reported `RELEASE.2025-04-22T22-12-26Z` as the last image with the full console. | [V-24] (Blocks & Files, 2025-06-19; community reports) |
| 2025-10 (around the 15 Oct security release) | Community Docker images and pre-built binaries **stopped**: "moved to a source-only distribution" (issue #21647). | [V-25] |
| 2025-12 (reported as Dec 3) | The repository was put in **maintenance mode**: no new features, PRs not reviewed, limited critical fixes. | [V-23b, I: exact day] |
| 2026-02 | The README changed to "THIS REPOSITORY IS NO LONGER MAINTAINED". Some sources date a first archive to Feb 12–13, 2026. | [I-23c] (secondary sources disagree) |
| **2026-04-25** | GitHub banner: "This repository was archived by the owner on Apr 25, 2026", "Public archive". The README points to **AIStor Free** (free-licensed standalone) and **AIStor Enterprise**. The source stays AGPLv3. Old binaries remain "but will not receive updates". | [V-23] |

**Conclusion:** do not ship MinIO CE in the reference deployment. It receives no security fixes and no official images, and community rebuilds such as `pgsty/minio` are third-party trust decisions. AIStor Free is a proprietary license and needs legal review [I]. Fleet should still *work against* MinIO or AIStor where IT already runs it, because it is only an S3 endpoint.

### 7.2 Comparison

Fleet needs only these S3 operations:

- Put, Get, Head, Delete and DeleteObjects
- ListObjectsV2
- multipart upload
- presigned GET and PUT (optional; see the proxy design below)
- lifecycle rules for `AbortIncompleteMultipartUpload` (nice to have), because Fleet enforces retention itself from Postgres

| Option | Language / license | Latest (verified) | Presigned | Multipart | Lifecycle | Ops footprint | Assessment |
|---|---|---|---|---|---|---|---|
| **Garage** (Deuxfleurs) | Rust / **AGPLv3** [V-39b] | **v2.5.0**, 2026-10-24 (Docker Hub `dxflrs/garage`) [V-40] | ✅ [V-39] | ✅ all endpoints [V-39] | ⚠ Partial: `Expiration` and `AbortIncompleteMultipartUpload` supported [V-39] | Single static binary, low RAM. Needs layout, key and bucket bootstrap. In production at Deuxfleurs since 2020 [V-39b] | **Recommended default.** Supports exactly the feature subset Fleet needs, is mature, and matches the Rust stack. No versioning, object lock or bucket policies [V-39], none of which Fleet needs. |
| **SeaweedFS** | Go / Apache-2.0 [I] | **4.52**, 2026-11-02 (Docker Hub `chrislusf/seaweedfs`) [V-40] | ✅ [I-41] | ✅ [I-41] | ✅ via a scheduled worker; versioned-rule gaps [I-41] | `weed server -s3` all-in-one mode is possible. More moving parts (master, volume, filer) at scale | **Alternative when AGPL is unacceptable.** Very active and long-lived. |
| **RustFS** | Rust / Apache-2.0 [V-42] | **1.0.4**, 2026-10-25. 1.0 GA in September 2026 [V-42, V-43] | ✅ [I: S3 API; not in README] | ✅ [I] | ✅ ILM [V-42] | MinIO-like single binary plus console. Default creds `rustfsadmin/rustfsadmin` [V-42] | **Watch.** About 6 weeks past GA. CVE-2025-68926 (CVSS 9.8, hardcoded gRPC token, fixed in alpha.78) [V-44]. Reconsider in 2027. |
| **MinIO CE** | Go / AGPLv3 | Archived 2026-04-25 [V-23] | ✅ | ✅ | ✅ | Source-only | **Not recommended** (§7.1). |
| **Ceph RGW** | C++ / LGPL [I] | — | ✅ | ✅ | ✅ | Heavy: a full Ceph cluster | Use only if IT already runs Ceph. Fleet just points at it. |
| **Zenko CloudServer** (Scality) | Node.js / Apache-2.0 [V-45] | Active development branch `development/9.6`, not archived [V-45] | ✅ [I] | ✅ [I] | ⚠ [I] | Single container `zenko/cloudserver`, file backend by default [V-45] | A viable small-scale option, but Node.js adds a second runtime and production references are fewer [I]. |
| **Fleet filesystem backend** | Rust (in Fleet) | — | Fleet-signed links | n/a (streamed) | Fleet retention job | Zero extra services. A volume only | **Fallback** for the smallest installs, demos and air-gapped pilots. Single node only. |
| **External S3** (AWS S3, GCS interop, Azure via gateway, NetApp, Dell ECS, …) | — | — | ✅ | ✅ | ✅ | IT-managed | **Preferred in production** whenever IT already provides S3. |

### 7.3 Recommendation [I]

- **Fleet storage abstraction:** use the `object_store` crate (Apache Arrow project), which covers S3-compatible endpoints and the local filesystem behind one trait. Or define a narrow `ArtifactStore` trait: `put_stream`, `get_stream`, `head`, `delete`, `list_prefix`, `presign_get`, `presign_put`. Spec §2 already demands "storage interfaces must not depend on a specific cloud provider." Use path-style addressing by default, because Garage and most self-hosted stores prefer it, with a configurable region. Garage uses `garage` as its region by default [I].
- **Default transfer mode: proxy through Fleet.** Agents upload to `POST /api/v1/artifacts/...` and Fleet streams to the store while enforcing quotas and validating type and checksum. Browsers download through Fleet with short-lived capability links. This keeps the object store internal, avoids exposing a second TLS hostname, and makes quota enforcement exact.
- **Optional direct mode:** presigned PUT and GET for large videos when IT exposes the store. Fleet still verifies size and SHA-256 after upload, via HEAD plus an optional re-hash job.
- **Reference Compose:** Garage v2.5.0 single node with `replication_factor = 1`, LMDB or SQLite metadata, and a one-shot init container that creates the layout, key and bucket.
- **Documented alternatives:** SeaweedFS (Apache-2.0), external S3, or the filesystem backend (`FLEET_STORAGE=fs:///var/lib/fleet/artifacts`).
- **Retention:** owned by Fleet. A DB-driven deletion job by retention class (spec §7) means behaviour is identical across backends. Configure the store's `AbortIncompleteMultipartUpload` (1 day) where supported, and also run a Fleet sweeper.

---

## 8. SSO (dashboard users)

### 8.1 IdP quirks relevant to roles

| IdP | Groups/roles in token | Quirks |
|---|---|---|
| **Microsoft Entra ID** | `groups` (object IDs by default; `sAMAccountName` only for AD-synced groups) or **`roles`** (app roles) [V-38] | **Overage:** more than 200 groups in a JWT means *no* `groups` claim, only `_claim_names`/`_claim_sources` (or `hasgroups` in implicit flow) [V-38]. `_claim_sources` may point at the legacy AAD Graph, so detect the claim's presence and call Microsoft Graph instead of using its value [V-38]. "Groups assigned to the application" avoids overage but excludes nested groups and requires P1 [V-38]. **Recommendation: use app roles**, e.g. `Fleet.Viewer` and `Fleet.Admin`, plus project-scoped roles such as `Fleet.Project.mobile-app.Maintainer` [V-38]. Group info is "current only when you receive the token" [V-38]. |
| **Okta** | `groups` claim must be configured (app-level groups-claim filter, or a custom authorization server claim) [I] | Group *names*, not IDs. Filters (regex/starts-with) limit claim size [I]. The org authorization server and custom servers differ in claim configuration [I]. |
| **Google Workspace** | **No groups in the ID token** [I-46]. `hd` gives the Workspace domain and must be checked when restricting by domain [V-46] | Map roles in Fleet by email, use the Cloud Identity Groups API (needs admin consent and scopes), or broker through Keycloak, Dex or Authentik [I-46]. |
| **Keycloak** | Groups via a "Group Membership" mapper, optionally with full path (`/eng/mobile`). Realm roles in `realm_access.roles`, client roles in `resource_access.<client>.roles` [I] | The full-path setting changes values, so mapping rules must state which format is used. |
| **Authentik** | `groups` claim from the default profile scope mapping [I] | Custom property mappings can emit roles [I]. |

### 8.2 Recommended configuration [I]

- OIDC Authorization Code with PKCE, as a confidential client. The user key is `(iss, sub)`, never email. Require `email_verified` if email is displayed. Use server-side sessions (Postgres) with an idle timeout of 8 h and an absolute timeout of 24 h, so group changes take effect at the next login.
- **Default deny:** a user who matches no rule gets no access, or `viewer_none`, a configurable landing state. Missing or overage group claims never fall through to a permissive default.
- **Mapping:**

```yaml
sso:
  issuer: https://login.microsoftonline.com/<tenant>/v2.0
  client_id: ...
  client_secret_file: /run/secrets/oidc_client_secret
  scopes: [openid, profile, email]
  claims:
    roles: roles            # JSON pointer-ish path; e.g. "realm_access.roles" for Keycloak
    groups: groups
  entra_overage: graph      # "graph" (needs GroupMember.Read.All) | "deny"
  rules:                    # first match per project wins; global rules apply to all projects
    - match: { claim: roles, equals: Fleet.Admin }
      grant: { role: administrator, projects: "*" }
    - match: { claim: groups, equals: "4f1c…-entra-object-id" }      # Entra: object IDs
      grant: { role: maintainer, projects: [mobile-app] }
    - match: { claim: groups, glob: "fleet-viewers-*" }              # Okta/Keycloak: names
      grant: { role: viewer, projects: "*" }
  bootstrap_admins: [ "https://login.microsoftonline.com/<tenant>/v2.0|<sub>" ]  # first-run only
  local_break_glass: false
```

- Spec §10 roles are Viewer, Maintainer and Administrator. Add an implicit **Publisher**, which is not a human role: it exists only as CI session credentials.
- Record role-mapping changes, logins and failed authorizations in `audit_events` (spec §10 Audit).

---

## 9. Docker Compose reference deployment

### 9.1 Services

| Service | Image | Role |
|---|---|---|
| `proxy` | `caddy:2` (2.11.x, 2026-10-30) [V-40] | TLS termination for **two hostnames** (app and user content) and WebSocket proxying. Caddy is recommended for its simple config, automatic or internal-CA TLS, no default stream timeouts and safe `X-Forwarded-*` defaults [V-34]. Traefik is fine for Docker-label shops, but raise or disable `readTimeout` for WS (default 60 s [V-32]). nginx is fine, but set `proxy_read_timeout` and the Upgrade headers [V-30]. |
| `fleet` | `ghcr.io/acme/maestro-fleet:1.4.2` | One Rust binary: axum API, WS ingestion, browser WS, embedded static web assets (spec §11 "may share one image"). Subcommands: `serve`, `migrate`, `healthcheck`, `admin`, `backup-check`. |
| `postgres` | `postgres:18.6` [V-35, V-40] | Durable state. |
| `garage` | `dxflrs/garage:v2.5.0` [V-40] | Artifact store (or external S3, or none with the fs backend). |
| `garage-init` | same | One-shot bootstrap of layout, key and bucket. |
| `backup` (optional) | `postgres:18.6` + cron or `pgbackrest` | Nightly `pg_dump -Fc` to a volume or S3. |

### 9.2 WebSocket and proxy pitfalls

| Hop | Default idle behaviour | Action |
|---|---|---|
| nginx | Closes if upstream sends nothing for **60 s** (`proxy_read_timeout`) [V-30] | Set `proxy_read_timeout 3600s; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";`. `proxy_http_version 1.1` is needed only before nginx 1.29.7 [V-30]. |
| AWS ALB | **60 s** idle timeout. HTTP/2 PINGs do *not* reset it [V-31] | Send data frames or WS pings at intervals under 60 s. |
| Cloudflare | Closes idle WS; reported as **100 s**. Enterprise can customize. Also drops WS on Cloudflare deploys [V-33, I] | Heartbeats and reconnect. |
| Traefik v3 | `respondingTimeouts.readTimeout` default **60 s** [V-32] | Set a dedicated entrypoint with larger or zero timeouts (verify behaviour for WS) [I]. |
| Caddy | No stream timeout by default, but **WS are force-closed on config reload** [V-34] | Set `stream_close_delay 5m`. Clients reconnect anyway. |
| Corporate forward proxies (Zscaler, Blue Coat, Squid) | Often 30–120 s idle timeouts. Some block WS or do TLS inspection [I] | Agent honours `HTTPS_PROXY`/`NO_PROXY` and a custom CA. **HTTPS batch-POST fallback transport** when WS upgrade fails [I]. |

**Rules [I]:**

- The agent sends a WS Ping every **20 s**. The server pings browsers every **25 s**. Both treat 2 missed pongs as dead and reconnect with jittered backoff.
- The server's heartbeat-expiry thresholds (spec §6 "stale") must be greater than 3× the ping interval.
- WS messages are idempotent with sequence numbers (spec §5/§13), so a proxy-induced reconnect is invisible.
- Rust specifics: `tokio-tungstenite` has no built-in HTTP CONNECT proxy support, so the agent needs an explicit CONNECT tunnel through `HTTPS_PROXY` (with `NO_PROXY` matching) before the TLS and WS handshake [I: verify crate state].
- Use `rustls` with `rustls-platform-verifier` or `rustls-native-certs` so corporate root CAs in the OS store work, plus `FLEET_CA_FILE` / `SSL_CERT_FILE` for an extra PEM bundle [I].

### 9.3 TLS and corporate CA

- **Proxy certificates:** IT-issued certificates mounted into Caddy (`tls /certs/fullchain.pem /certs/key.pem`), ACME against an internal CA (`acme_ca https://acme.corp/...`), or `tls internal` for pilots [I].
- **Fleet outbound TLS** (OIDC JWKS from GHES/GitLab/Jenkins, IdP discovery, external S3) must trust the corporate CA: mount the PEM and set `FLEET_EXTRA_CA_FILE`.
- **Agent on runners:** same mechanism. `maestro-fleet check` validates DNS, TLS chain, proxy, WS upgrade, clock skew (which matters for JWT `iat`) and token minting, and prints remediation hints.

### 9.4 Health, migrations, shutdown [I]

- **Endpoints:**
  - `/healthz` is liveness: the process is up and the event loop responds.
  - `/readyz` is readiness: DB reachable, migrations at the expected version, object store reachable (HEAD on the bucket) and JWKS cache warm (or a degraded flag).
  - `fleet healthcheck` calls `/readyz` over localhost for Docker `HEALTHCHECK`, because the distroless image has no curl.
- **Migrations:** `sqlx` migrations embedded in the binary.
  - `FLEET_MIGRATE=auto` (Compose default): run on start under a Postgres advisory lock, so concurrent replicas are safe.
  - `FLEET_MIGRATE=manual` (IT-controlled production): `docker compose run --rm fleet migrate`. `serve` refuses to start if the schema is older than required, **or newer than the binary knows**, which prevents a silent rollback onto a newer schema.
  - Migrations are forward-only, with expand/contract steps for anything needing zero downtime later.
- **Graceful shutdown on SIGTERM:**
  1. Stop accepting new WS.
  2. Send agents a `server_draining` control message so they buffer and reconnect.
  3. Flush in-flight batches.
  4. Exit within `stop_grace_period`.

  Spec §13 "Backend restart": state is rebuilt from Postgres.

### 9.5 PostgreSQL version, backup and restore

- **Version: PostgreSQL 18** (18.6 current; supported until 2030-11-14) [V-35]. PG 19 is at `19rc1` (2026-11-01) [V-36]. Plan a move to 19 in 2027 after 19.1 or 19.2. PG 14 reaches EOL on 2026-11-12 [V-35].
- **PG 18 image change:** default `PGDATA` is `/var/lib/postgresql/18/docker`. Mount the volume at **`/var/lib/postgresql`**. Old `/var/lib/postgresql/data` mounts fail [V-37].
- **Backups:**
  - **Default:** nightly `pg_dump -Fc` with 14 daily and 8 weekly copies, written to a volume and optionally copied to S3. This is simple, version-portable, and adequate at Fleet's scale (tens of GB).
  - **PITR option:** pgBackRest **2.60.0** (2026-10-18; PG 19 support since 2.59) [V-47], with an S3 repository. Prefer it over WAL-G, whose last release is 3.0.8 (2026-01-26) [V-48].
  - **Object store:** Garage has no versioning [V-39]. Back up artifacts with `rclone sync` to a second target, or accept artifact loss as lower-impact than DB loss (document this).
  - **Restore order:** DB first, then objects. Fleet runs an **orphan reconciler** that marks artifacts without objects as `missing` and deletes objects without rows after a grace period.
  - **Restore drill:** `fleet backup-check` restores the latest dump into a scratch DB and runs schema and row-count checks. Recommend it monthly.
- **Upgrades:**
  - Fleet uses semver. The agent↔server protocol is versioned in the WS handshake, and the server supports the current and previous minor protocol.
  - Upgrade order: back up, upgrade the server (migrations run), then bump the agent version in workflows.
  - Postgres major upgrades: `pg_dump`/`pg_restore` into a new volume (simple) or `pg_upgrade` (faster). Pin the major version in Compose (`postgres:18.6`, never `latest`).

### 9.6 `docker-compose.yml` sketch [I]

```yaml
name: maestro-fleet

x-restart: &restart { restart: unless-stopped }

services:
  proxy:
    <<: *restart
    image: caddy:2.11
    ports: ["443:443", "80:80"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./certs:/certs:ro                 # IT-issued certs (or use ACME/internal CA)
      - caddy_data:/data
    depends_on:
      fleet: { condition: service_healthy }

  fleet:
    <<: *restart
    image: ghcr.io/acme/maestro-fleet:1.4.2
    command: ["serve"]
    environment:
      FLEET_PUBLIC_URL: https://fleet.example.com
      FLEET_USERCONTENT_URL: https://fleet-usercontent.example.net
      FLEET_DATABASE_URL_FILE: /run/secrets/database_url
      FLEET_MIGRATE: auto                 # "manual" for IT-controlled prod
      FLEET_STORAGE: s3
      FLEET_S3_ENDPOINT: http://garage:3900
      FLEET_S3_REGION: garage
      FLEET_S3_BUCKET: fleet-artifacts
      FLEET_S3_FORCE_PATH_STYLE: "true"
      FLEET_S3_ACCESS_KEY_ID_FILE: /run/secrets/s3_access_key_id
      FLEET_S3_SECRET_ACCESS_KEY_FILE: /run/secrets/s3_secret_access_key
      FLEET_ARTIFACT_TRANSFER: proxy      # "proxy" (default) | "presigned"
      FLEET_CONFIG_FILE: /etc/fleet/fleet.toml   # SSO, publishers, retention, quotas
      FLEET_EXTRA_CA_FILE: /etc/fleet/corp-ca.pem
      FLEET_WS_PING_INTERVAL: 25s
      FLEET_LOG_FORMAT: json
      RUST_LOG: info
    volumes:
      - ./fleet.toml:/etc/fleet/fleet.toml:ro
      - ./corp-ca.pem:/etc/fleet/corp-ca.pem:ro
      # - fleet_artifacts:/var/lib/fleet/artifacts   # when FLEET_STORAGE=fs
    secrets: [database_url, s3_access_key_id, s3_secret_access_key, oidc_client_secret, fleet_signing_key]
    depends_on:
      postgres: { condition: service_healthy }
      garage-init: { condition: service_completed_successfully }
    healthcheck:
      test: ["CMD", "/usr/local/bin/fleet", "healthcheck"]
      interval: 10s
      timeout: 3s
      retries: 6
      start_period: 20s
    stop_grace_period: 30s
    expose: ["8080"]                       # internal only; proxy is the sole ingress

  postgres:
    <<: *restart
    image: postgres:18.6
    environment:
      POSTGRES_DB: fleet
      POSTGRES_USER: fleet
      POSTGRES_PASSWORD_FILE: /run/secrets/postgres_password
    volumes:
      - pg_data:/var/lib/postgresql          # PG18+: mount the parent, not .../data
    secrets: [postgres_password]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U fleet -d fleet"]
      interval: 5s
      retries: 12
    shm_size: 256mb

  garage:
    <<: *restart
    image: dxflrs/garage:v2.5.0
    volumes:
      - ./garage.toml:/etc/garage.toml:ro   # replication_factor=1, rpc_secret, admin token, s3 region "garage"
      - garage_meta:/var/lib/garage/meta
      - garage_data:/var/lib/garage/data
    expose: ["3900", "3903"]                # S3 API, admin API — internal only

  garage-init:                               # idempotent bootstrap; verify commands vs Garage 2.5 docs
    image: dxflrs/garage:v2.5.0
    depends_on: [garage]
    entrypoint: ["/bin/sh", "-c"]            # NOTE: upstream image is minimal; a tiny init image
    command: ["/scripts/garage-init.sh"]     #   with the garage CLI + sh may be needed
    volumes:
      - ./garage.toml:/etc/garage.toml:ro
      - ./scripts:/scripts:ro
    secrets: [s3_access_key_id, s3_secret_access_key]
    restart: "no"

  backup:
    <<: *restart
    image: postgres:18.6
    profiles: ["backup"]
    entrypoint: ["/scripts/pg-backup-loop.sh"]   # nightly pg_dump -Fc + retention pruning
    environment: { PGHOST: postgres, PGUSER: fleet, PGDATABASE: fleet }
    volumes: ["./scripts:/scripts:ro", "pg_backups:/backups"]
    secrets: [postgres_password]

volumes: { caddy_data: {}, pg_data: {}, pg_backups: {}, garage_meta: {}, garage_data: {} }

secrets:
  database_url:        { file: ./secrets/database_url }
  postgres_password:   { file: ./secrets/postgres_password }
  s3_access_key_id:    { file: ./secrets/s3_access_key_id }
  s3_secret_access_key:{ file: ./secrets/s3_secret_access_key }
  oidc_client_secret:  { file: ./secrets/oidc_client_secret }
  fleet_signing_key:   { file: ./secrets/fleet_signing_key }
```

`Caddyfile` sketch [I]:

```caddyfile
{
  servers { trusted_proxies static private_ranges }   # only if behind another LB
}

fleet.example.com {
  tls /certs/fleet.crt /certs/fleet.key
  encode zstd gzip
  reverse_proxy fleet:8080 {
    stream_close_delay 5m          # don't drop WS on config reload
    flush_interval -1              # low latency for streaming endpoints
  }
}

fleet-usercontent.example.net {
  tls /certs/usercontent.crt /certs/usercontent.key
  reverse_proxy fleet:8080         # Fleet routes by Host; only /u/* capability URLs served here,
}                                  # with CSP sandbox + nosniff headers set by Fleet itself
```

---

## 10. Agent binary supply chain

- **Build:** `cargo build --release --locked` on GitHub-hosted runners in a dedicated release workflow. Targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin` (until Intel Macs are retired) [I]. Use `cargo-auditable` and `cargo-deny` in CI [I].
- **Provenance:** `actions/attest@v4` creates a SLSA build-provenance attestation by default. It needs `id-token: write`, `attestations: write` and `artifact-metadata: write` [V-49]. `actions/attest-build-provenance` is now a wrapper, and new implementations should use `actions/attest` [V-50]. Attestations in **private or internal repos need GitHub Enterprise Cloud** and use GitHub's private Sigstore instance [V-50]. Public repos use public-good Sigstore. Verify with `gh attestation verify <file> --repo acme/maestro-fleet` (optionally `--signer-workflow`) [I: flags from gh manual, verify].
- **Immutable releases** (GA 2025-10-28 [I-51]): enable them so tags and assets cannot be replaced after publication.
- **Checksums:** publish `SHA256SUMS`, plus `SHA256SUMS.sigstore.json` or a cosign bundle [I].
- **`setup-maestro-fleet` action** (composite or JavaScript) [I]:
  - Inputs: `version` (required and exact; no `latest` by default), `verify-attestation` (default `false` for portability, `true` recommended), `download-base` (default GitHub Releases).
  - **Embeds the SHA-256 of every released asset per version** inside the action source, so pinning the action by commit SHA pins the binary transitively, without trusting the download host.
  - Caches the binary in the runner tool cache.
  - Self-hosted and air-gapped installs: `download-base: https://fleet.example.com/agent` lets the **Fleet server serve the agent binaries matching its own version**. That improves version compatibility and avoids internet egress from runners. Checksums are still verified against the action's embedded list.
  - Users pin the action by full commit SHA (GitHub's guidance for third-party actions [I]). Dependabot or Renovate bump them.
- **Non-GitHub CI:** an `install.sh` that requires `--version` and `--sha256` and refuses to run without both. An optional `cosign verify-blob` path [I]. Container image `ghcr.io/acme/maestro-fleet-agent:<version>` for Docker-based CI, signed with cosign keyless [I].

---

## 11. Proposed spec changes (for the decisions log)

1. **§3/§7 identity:**
   - Pipeline key `(provider, server_url, repository_id, run_id, run_attempt)`.
   - Job key `check_run_id` (GitHub), `CI_JOB_ID` (GitLab) or `BUILDKITE_JOB_ID`, with a synthetic fallback and an `identity_quality` flag.
   - Store `tested_sha` and `head_sha` separately.
2. **§10 CI publisher authentication:**
   - OIDC token exchange for a write-only session credential.
   - Authorize on immutable numeric claims, never on `sub`.
   - Trust levels `trusted` / `pr` / `untrusted`.
   - Fork PRs run offline by default, with an optional `workflow_run` relay.
   - `pull_request_target` is explicitly unsupported.
3. **§10 Sensitive data:**
   - Known-secret set (argv `-e`, `MAESTRO_*`, secret-named env vars) plus gitleaks-style patterns, applied in the agent and again on the server.
   - Documented guidance: secrets go in `MAESTRO_*` env, not `-e`.
   - User HTML only on a separate registrable domain with `CSP: sandbox`, or download only.
4. **§11 Deployment:**
   - Single `fleet` image; Caddy reference proxy; Postgres 18; Garage default object store with SeaweedFS and filesystem alternatives; MinIO CE explicitly not recommended.
   - Fleet-proxied artifact transfer by default.
   - `FLEET_MIGRATE=auto|manual`.
   - `fleet healthcheck` subcommand.
5. **§5 transport:**
   - Ping every 20–25 s; heartbeat expiry greater than 3× the interval.
   - CONNECT-proxy and custom-CA support in the agent.
   - HTTPS batch-POST fallback when WS is blocked.
6. **§12 adapters:** GitHub, plus Generic `FLEET_*` with overrides in v1. GitLab next (cleanest OIDC: `ref_protected`, retry count). Buildkite and CircleCI after. Jenkins through the OIDC Provider plugin or a static token.
7. **Runner guidance doc:** Linux x64 with KVM, or self-hosted Apple Silicon with arm64 images. No macOS arm64 hosted runners. Emulator flag presets and per-emulator resource sizing.
8. **§4 agent selftest:** check KVM/HVF acceleration, CA trust, proxy, clock skew and OIDC availability, with explicit "publishing disabled because …" messages.

---

## Sources

Retrieved or checked on 2026-11-03 unless noted. "V-n" in the text refers to these entries.

1. [V-1] GitHub Docs: Variables reference. https://docs.github.com/en/actions/reference/workflows-and-actions/variables
2. [V-2] GitHub Docs: OpenID Connect reference (claims, sub formats, customization, request env vars). https://docs.github.com/en/actions/reference/security/oidc. [V-2b] GitHub Docs: Contexts reference (job.check_run_id, runner.environment). https://docs.github.com/en/actions/reference/workflows-and-actions/contexts
3. [V-3] github/docs issue #40669: `job.check_run_id` added 2025-09-12 (commit 2d8b1f0), fpt/ghec only; example ID matches the job URL. https://github.com/github/docs/issues/40669
4. [V-4] GitHub Changelog, 2025-11-13: OIDC token claims now include `check_run_id`. https://github.blog/changelog/2025-11-13-github-actions-oidc-token-claims-now-include-check_run_id/
5. [V-5] GitHub Changelog, 2026-04-23 (edited 2026-06-10): Immutable subject claims for GitHub Actions OIDC tokens. https://github.blog/changelog/2026-04-23-immutable-subject-claims-for-github-actions-oidc-tokens
6. [V-6] GitHub Docs: Events that trigger workflows (pull_request SHA/ref, forks, pull_request_target warnings). https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows
7. [V-7] GitHub Docs: Workflow syntax, `permissions` (fork downgrade, "Send write tokens…", id-token). https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax
8. [V-8] GitHub Changelog, 2025-11-07: Actions pull_request_target and environment branch protections changes (effective 2025-12-08). https://github.blog/changelog/2025-11-07-actions-pull_request_target-and-environment-branch-protections-changes/
9. [V-9] GitHub Enterprise Server docs: OIDC reference (issuer `https://HOSTNAME/_services/token`, enterprise claims). https://docs.github.com/en/enterprise-server@3.17/actions/reference/security/oidc
10. [I-10] Spacelift support: GitHub OIDC token expires after 5 minutes (third party). https://support.spacelift.io/articles/8847614416-why-does-my-github-oidc-token-expire-after-5-minutes-when-using-spacelift-api
11. [I-11] Argos CI docs: GitHub OIDC not issued for forks (third party). https://argos-ci.com/docs/github-oidc
12. [V-12] GitHub Docs: Secure use reference (pull_request_target/workflow_run, script injection, self-hosted runners on public repos). https://docs.github.com/en/actions/reference/security/secure-use
13. [V-13] actions/runner-images issue #9741 (Android emulator on macOS arm64). https://github.com/actions/runner-images/issues/9741
14. [V-14] GitHub Docs: GitHub-hosted runners reference (specs, Android hardware acceleration, macOS arm64 nested virtualization). https://docs.github.com/en/actions/reference/runners/github-hosted-runners
15. [V-15] ReactiveCircus/android-emulator-runner README and releases (KVM udev rule, defaults, caching; v2.38.0). https://github.com/ReactiveCircus/android-emulator-runner, https://github.com/ReactiveCircus/android-emulator-runner/releases
16. [V-16] Android Developers: Start the emulator from the command line (flags, ports 5554–5682, -accel, deprecation in favour of the Android CLI). https://developer.android.com/studio/run/emulator-commandline
17. [I-17] Intel macOS runner retirement, fall 2027 (secondary). https://discuss.python.org/t/dropping-intel-mac-to-tier-2/102100/1
18. [V-18] GitLab Docs: Predefined CI/CD variables. https://docs.gitlab.com/ci/variables/predefined_variables/
19. [V-19] GitLab Docs: OpenID Connect (OIDC) authentication using ID tokens. https://docs.gitlab.com/ci/secrets/id_token_authentication/
20. [V-20] Maestro Docs: Parameters and constants (`-e`, `MAESTRO_*`); CLI commands and options. https://docs.maestro.dev/maestro-flows/flow-control-and-logic/parameters-and-constants, https://docs.maestro.dev/maestro-cli/maestro-cli-commands-and-options, https://docs.maestro.dev/maestro-cli/environment-variables
21. [V-21] Buildkite Docs: `buildkite-agent oidc`. https://buildkite.com/docs/agent/v3/cli-oidc
22. [V-22] CircleCI Docs: OpenID Connect tokens. https://circleci.com/docs/guides/permissions-authentication/openid-connect-tokens/
23. [V-23] minio/minio GitHub repository (archived 2026-04-25 banner, README). https://github.com/minio/minio. [V-23b] dev.to: MinIO in maintenance mode (2025-12-31). https://dev.to/alexandrev/minio-in-maintenance-mode-what-it-means-for-the-community-edition-oem-users-and-open-source-3o4j. [I-23c] Glukhov: MinIO CE in 2026 (timeline, secondary). https://www.glukhov.org/data-infrastructure/object-storage/minio-dead/
24. [V-24] Blocks & Files, 2025-06-19: MinIO removes management features from community edition. https://blocksandfiles.com/2025/06/19/minio-removes-management-features-from-basic-community-edition-object-storage-code/
25. [V-25] Gigazine, 2025-10-23: MinIO stops distributing free Docker images (issue #21647). https://gigazine.net/gsc_news/en/20251023-minio-stops-distributing-free-docker-images
26. [V-26] Jenkins plugin: OpenID Connect Provider. https://plugins.jenkins.io/oidc-provider/
27. [V-27] actions/checkout releases (v7.0.1 latest). https://github.com/actions/checkout/releases
28. [I-28] Codersera: Android emulator in Docker without KVM (ARM runners lack /dev/kvm; third party). https://codersera.com/blog/android-emulator-docker-without-kvm/
29. [V-29] AOSP prebuilts: `qemu-system-x86_64-headless`. https://android.googlesource.com/platform/prebuilts/android-emulator/+/ead58c3962064228298f2a7b09dce724126f069f/linux-x86_64/qemu/linux-x86_64/qemu-system-x86_64-headless
30. [V-30] nginx: WebSocket proxying (60 s default). https://nginx.org/en/docs/http/websocket.html
31. [V-31] AWS: ALB attributes (idle timeout 60 s; HTTP/2 PINGs do not reset it). https://docs.aws.amazon.com/elasticloadbalancing/latest/application/edit-load-balancer-attributes.html
32. [V-32] Traefik v3 entrypoints (readTimeout default 60 s). https://doc.traefik.io/traefik/v3.5/reference/install-configuration/entrypoints/
33. [V-33] Cloudflare: WebSockets. https://developers.cloudflare.com/network/websockets/ (the 100 s figure comes from third parties, e.g. https://websocket.org/guides/infrastructure/cloudflare/)
34. [V-34] Caddy: `reverse_proxy` directive (stream_timeout, stream_close_delay, trusted_proxies). https://caddyserver.com/docs/caddyfile/directives/reverse_proxy
35. [V-35] PostgreSQL versioning policy (18.6 current; EOL dates). https://www.postgresql.org/support/versioning/
36. [V-36] Docker Hub `library/postgres` tags (18.6 on 2026-10-31; 19rc1 on 2026-11-01). https://hub.docker.com/_/postgres
37. [V-37] docker-library/docs PR #2582: PG 18+ PGDATA and volume change. https://github.com/docker-library/docs/pull/2582/files
38. [V-38] Microsoft Learn: Configure group claims and app roles in tokens (overage 200/150/6, `_claim_names`, app roles). https://learn.microsoft.com/en-us/security/zero-trust/develop/configure-tokens-group-claims-app-roles
39. [V-39] Garage: S3 compatibility reference. https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/. [V-39b] Garage README (AGPLv3, in production since 2020). https://docs.rs/crate/garage/0.9.0/source/README.md
40. [V-40] Docker Hub tag listings queried 2026-11-03: `dxflrs/garage` (v2.5.0, 2026-10-24), `chrislusf/seaweedfs` (4.52, 2026-11-02), `rustfs/rustfs` (1.0.4, 2026-10-25), `library/caddy` (2.11.7, 2026-10-30). https://hub.docker.com/
41. [I-41] SeaweedFS wiki (S3 API, S3 Lifecycle; mirror). https://github.com/seaweedfs/seaweedfs/wiki
42. [V-42] rustfs/rustfs README (Apache-2.0, features, default credentials). https://github.com/rustfs/rustfs
43. [V-43] Linuxiac: RustFS 1.0 reaches GA (September 2026). https://linuxiac.com/rustfs-1-0-s3-compatible-object-storage-reaches-ga/
44. [V-44] GitLab Advisory Database: CVE-2025-68926 (RustFS hardcoded gRPC token). https://advisories.gitlab.com/pkg/cargo/rustfs/CVE-2025-68926/
45. [V-45] scality/cloudserver repository. https://github.com/scality/cloudserver
46. [V-46] Google Identity: OpenID Connect reference (`hd` claim). https://developers.google.com/identity/openid-connect/reference. Groups-claim absence (third party): https://docs.upbound.io/hub/iam/identity/google-workspace/
47. [V-47] PostgreSQL news: pgBackRest 2.60.0 released (2026-10-18); 2.59.0 (PG 19 support). https://www.postgresql.org/about/news/pgbackrest-2600-released-3416/, https://postgresql.org/about/news/pgbackrest-2590-released-3355
48. [V-48] PostgreSQL news: WAL-G 3.0.8 released (2026-01-26). https://www.postgresql.org/about/news/wal-g-308-released-3219
49. [V-49] actions/attest README (v4, permissions, SLSA provenance default). https://github.com/actions/attest
50. [V-50] actions/attest-build-provenance README (wrapper since v4; private repos need GHEC). https://github.com/actions/attest-build-provenance
51. [I-51] Immutable releases GA, 2025-10-28 (secondary summary; primary: https://github.blog/changelog/2025-10-28-immutable-releases-are-now-generally-available/). https://sysdesign-wiki.pages.dev/sources/2025-10-31-github-immutable-releases-ga/

**Not verified today. Check before implementation:**

- the exact GitHub OIDC token lifetime
- whether `job`, `matrix` and `strategy` contexts are usable inside a composite action's defaults
- `gh attestation verify` flag names
- the Okta, Keycloak and Authentik claim specifics
- SeaweedFS and CloudServer feature details
- Garage 2.5 single-node bootstrap commands (a single-node mode was reportedly added in 2026)
- `tokio-tungstenite` proxy support
- gitleaks' license and rule format
- the `MAESTRO_VERSION` install-script variable
