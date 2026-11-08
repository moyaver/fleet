# Operator guide

## Deploy

The reference deployment is Docker Compose: Caddy (TLS, WebSocket-friendly), the Fleet server, Postgres 18 and Garage (S3-compatible, used for artifacts from M5). Step-by-step: [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md). The image is built from [`deploy/docker/Dockerfile`](https://github.com/moyaver/fleet/blob/main/deploy/docker/Dockerfile) (cargo-chef, distroless, runs as non-root). Each release tag publishes it to `ghcr.io/moyaver/fleet`, tagged with the version (and `latest` for stable releases); pin a version with `FLEET_IMAGE` in the compose `.env`. For a single free cloud VM, see [Try Fleet on a free VM](free-vm.md).

## The `fleet` binary

| Command | Does |
|---|---|
| `fleet serve` | Serves the dashboard, the agent protocol and the API |
| `fleet migrate` | Applies database migrations and exits |
| `fleet check-config` | Validates the configuration and prints it, secrets redacted |
| `fleet healthcheck` | Exits 0 if the local server is ready (`/readyz`); for Docker `HEALTHCHECK` |
| `fleet api-key create --project <slug> --name <n> [--expires-in-days <d>]` | Prints a new API key once (`mf_live_…`); only its hash is stored. It expires after `d` days, or `api.api_key_ttl_days` (90) when omitted |
| `fleet api-key list --project <slug>` | Prints each key's ID, name, prefix, expiry, revocation and last use, tab-separated. No secret is shown |
| `fleet api-key revoke --id <id>` | Revokes a key at once, and every agent token minted from it stops with it. Works on keys made before expiry existed. Revoking a revoked key is fine |
| `fleet api-key expire --id <id> --in-days <d>` | Sets when a key stops working, `d` days from now (`0` ends it now). For keys made before expiry existed, or to shorten one. Refused for a revoked key |

## Configuration

Defaults, then a TOML file (`--config` or `FLEET_CONFIG`), then `FLEET_*` variables (`__` separates sections: `FLEET_DATABASE__URL`), then `FLEET_*_FILE` variables naming a file that holds the value (Docker and Kubernetes secrets). Errors name the variable or key at fault.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Address to listen on (the image sets `0.0.0.0:8080`) |
| `trusted_proxies` | `[]` | CIDR blocks of reverse proxies; their `X-Forwarded-For` names the client for the limits below. See [Client addresses and limits](#client-addresses-and-limits) |
| `rate_limits.*` | see below | Per-IP and per-credential request limits and the per-socket event rate |
| `database.url` | — | `postgres://…` (use `FLEET_DATABASE__URL_FILE`) |
| `database.migrate` | `auto` | `auto` migrates on start; `manual` refuses an outdated or newer schema |
| `ingest.heartbeat_interval_ms` | `10000` | Agent heartbeat cadence (sent to agents in `welcome`) |
| `ingest.ping_interval_ms` | `20000` | Server pings, below common 60 s proxy idle cuts |
| `ingest.idle_timeout_ms` | `45000` | A silent agent socket is closed and its run detached |
| `ingest.telemetry_flush_ms` | `15000` | Telemetry checkpoint (10–30 s), then `telemetry_ack` |
| `ingest.shutdown_grace_ms` | `15000` | Drain time for sockets on SIGTERM |
| `ingest.max_frame_bytes` | `262144` | Largest WebSocket frame and batch request accepted (256 KiB, research 06 T12). Agents fill each frame up to this and never send more; an agent event over 192 KiB is stored as a gap instead |
| `liveness.stale_after_ms` | `25000` | A connected agent silent this long shows `stale` in the dashboard (display only) |
| `liveness.resume_grace_ms` | `900000` | A detached run is abandoned; its open flows become `unknown`, and their open steps become `unknown` and Inferred |
| `api.viewer_tokens` | — | Bearer tokens that may read every project (scripts, tests); people sign in with `[login]` |
| `api.api_key_ttl_days` | `90` | Days until a new API key expires when `fleet api-key create` gets no `--expires-in-days`. Keys made before this setting have no expiry; set `expires_at` on them in SQL |
| `api.public_url` | — | The dashboard's origin: browser sockets from other origins are refused, and the sign-in redirect URI is built from it (required with `[login]`) |
| `login.*` | — | Browser sign-in with your identity provider; see [Browser sign-in](#browser-sign-in) |
| `storage.backend` | `local_fs` | Where artifacts live: `local_fs` or `s3`; see [Artifact storage](#artifact-storage) |
| `storage.path` | `/var/lib/fleet/blobs` | Blob directory for `local_fs` |
| `storage.s3.*` | — | Endpoint, region, bucket and credentials for `s3` |
| `storage.s3.public_endpoint` | — | Base URL agents reach the bucket at, for direct uploads; unset uses `endpoint` (see [Direct upload](#direct-upload)) |
| `artifacts.spool_dir` | `/var/lib/fleet/spool` | Where uploaded bytes wait for verification (local disk; one server instance per spool) |
| `artifacts.grant_ttl_seconds` | `600` | How long an upload grant lasts |
| `artifacts.max_run_bytes`, `artifacts.max_run_files` | 2 GiB, 2000 | What one agent run may store |
| `artifacts.max_project_bytes_per_day`, `artifacts.max_project_files_per_day` | 20 GiB, 20000 | What one project may declare in any 24 hours across all its runs. A refused upload says `project_quota_exceeded` |
| `artifacts.max_bytes.<class>` | 20 MiB screenshot, 16 MiB commands, 4 MiB failure_hierarchy, 512 KiB device_logcat, 32 MiB junit, 64 MiB log, 128 MiB html report | Size limit per class. `failure_hierarchy` is a step's UI hierarchy and `device_logcat` a failed flow's logcat excerpt |
| `retention.raw_samples_days` | `7` | Raw 5 s telemetry samples; see [Retention](#retention) (0 keeps for ever, like every `retention.*_days`) |
| `retention.rollup_1m_days`, `retention.rollup_1h_days` | `90`, `365` | One-minute and one-hour rollups |
| `retention.journal_days`, `retention.steps_days` | `14`, `14` | The raw agent journal and step executions |
| `retention.audit_days` | `365` | The audit log |
| `retention.blob_grace_hours` | `24` | How long a blob no artifact refers to is kept before it is deleted |
| `retention.expired_artifact_days` | `30` | How long an expired artifact's row stays as a tombstone for history |
| `retention.stale_upload_hours` | `24` | Temp files of crashed uploads older than this are removed (local store) |
| `retention.rollup_interval_seconds`, `retention.gc_interval_minutes` | `60`, `10` | How often rollups run, and artifacts expire and blobs are collected |
| `usercontent.url` | — | The usercontent origin; see [Usercontent domain](#usercontent-domain) |
| `usercontent.signing_key` | — | HMAC key for signed links, at least 32 bytes (`FLEET_USERCONTENT__SIGNING_KEY_FILE`) |
| `usercontent.link_ttl_seconds` | `300` | How long a signed link works (1 to 300) |
| `usercontent.listen` | — | Also serve the usercontent origin on its own address |
| `oidc.issuers` | GitHub, audience `fleet` | CI OIDC issuers accepted by `POST /api/v1/agent/token`. Each has an `audience` and optional `jwks_hosts`: the hosts, besides the issuer's own, that its discovery document's `jwks_uri` may name. A `jwks_uri` on any other host is refused and never fetched (research 06 T14) |
| `projects[].trust_policies[]` | — | Who may publish with an OIDC token (repository and owner IDs, trusted refs, PR policy) |

## Browser sign-in

People sign in to the dashboard through your identity provider (OpenID Connect, authorization code with PKCE). Register Fleet with the redirect URI `<api.public_url>/api/v1/auth/callback`; it is a confidential client when `client_secret` is set and a public (PKCE-only) client when it is not. The provider must sign ID tokens with RS256 (Entra ID, Okta, Keycloak and Google do by default) and must send the user's groups in the ID token.

```toml
[api]
public_url = "https://fleet.example.com"

[login]
issuer = "https://login.example.com/realms/corp"   # discovery: <issuer>/.well-known/openid-configuration
client_id = "fleet"
# client_secret: set FLEET_LOGIN__CLIENT_SECRET_FILE; omit for a public (PKCE-only) client
group_claims = ["groups", "roles"]                  # "roles" carries Entra ID app roles
session_ttl_minutes = 480                           # a session ends this long after sign-in at most
session_idle_minutes = 60                           # ...or after this long without a request

[[login.group_roles]]                               # one project
group = "mobile-qa"
role = "viewer"                                     # viewer | maintainer | admin
project = "mobile-app"

[[login.group_roles]]                               # no project: every project (admin: a global admin)
group = "fleet-admins"
role = "admin"
```

- **Roles come from groups, at sign-in.** A user's effective role on a project is the highest role granted by their groups (or bound to them directly) for that project or for all projects. A user whose groups grant nothing is refused, and so is an ID token with none of the `group_claims`: Fleet never guesses. Entra ID omits `groups` for users in more than 200 groups; use app roles (`roles`) there. A removed group membership takes effect at the next sign-in, within `session_ttl_minutes`. A session also ends after `session_idle_minutes` (default 60) without a request, and a request renews it. An open live socket does not renew it: its recheck ends the socket once the session has gone idle (research 06 T21).
- **Sessions** are an opaque cookie (`__Host-fleet_session`: `HttpOnly`, `Secure`, `SameSite=Lax`) stored in Postgres by hash. Mutating requests made with the cookie must send the session's CSRF token (`X-CSRF-Token`, from `GET /api/v1/auth/session`) and, when the browser sends an `Origin`, the dashboard's own.
- **Serve the dashboard over HTTPS.** The session cookie is `Secure` and carries the `__Host-` prefix, which browsers accept only from a secure origin, so sign-in cannot work over plain `http://` (browsers treat `localhost` and `127.0.0.1` as secure, which is how the end-to-end tests run). Put TLS in front of Fleet (the Compose file's Caddy does) and set `api.public_url` to the `https://` address people use.
- A project the user has no role on looks exactly like a project that does not exist.

## Administration

Global administrators see **Administration** in the navigation (`/admin`), and the audit history at `/admin/audit`. Every other person, including a project's own admin, a viewer token and anonymous readers, is refused on those URLs and in the API. Grant the role with `[[login.group_roles]]` and no `project`, or bind it to a person as a role for every project.

The page is **read only** in this release. It shows, one section per area:

| Section | Where the data lives | Changed by |
|---|---|---|
| Projects (onboarding) | `projects`, with each project's working API keys and trust policies | `projects` in the configuration (at start-up) |
| Publisher trust | `publisher_trust_policies` | `projects[].trust_policies` (replaced at start-up) |
| API keys | `api_keys`: name, prefix, trust level, expiry, revocation, last use. The secret and its hash are never shown | `fleet api-key create|revoke|expire` |
| Role mappings | `role_bindings` (people) and `group_mappings` (identity-provider groups) | `[[login.group_roles]]` at sign-in; bindings in SQL |
| Retention | `retention_classes` per project, and the `retention.*` windows of the configuration | the `retention_classes` table (SQL) and `retention.*` |
| Artifact policy | the `storage`, `artifacts` and `usercontent` settings of the running configuration | the configuration file, then a restart |
| Metric collection | the `ingest` checkpoint and heartbeat and the `retention` windows of the configuration. The agents sample every 5 s, and every 15 s under host pressure; no server setting changes that | the configuration file, then a restart |
| Agent compatibility | `agent_runs`: each agent version and protocol seen, with its open runs. Compatible means the protocol is the one this server speaks | agents are upgraded outside Fleet |
| Ingestion health | `ingest_rejections` (refused events), `ingest_gaps` (losses the agents reported) and the open runs with how long since the server last heard from them and how far behind their newest event was | |
| Audit history | `audit_events`, newest first, 50 at a time. Each event shows who (a person, an API key, or the system), what, when, and the values it changed where one was recorded (`before` and `after`, or the `from` and `to` of a triage change) | recorded by Fleet; kept for `retention.audit_days` |

Values from the configuration are shown with their key and marked `config`, so an operator can see what the running server uses. Until the forms arrive, a change to a table-backed area is made with SQL, as described in [Retention](#retention) and [API keys](#configuration).

## Client addresses and limits

Pre-auth requests (`POST /api/v1/agent/token`, browser sign-in and its callback) are limited per client IP, and each agent credential is limited on its WebSocket upgrades and batch requests. Each viewer credential (a configured viewer token, or a browser session) is limited on the REST reads and the live socket's upgrades. A refused request gets `429 Too Many Requests` with `Retry-After` in seconds. The agent waits that long and keeps spooling, so a limit slows a run's upload and never fails the run; the dashboard waits and keeps the data it has on screen.

`trusted_proxies` says which peers are proxies. Behind a reverse proxy the peer is the proxy, so the client is named by `X-Forwarded-For`; Fleet reads it only when the peer is inside a listed block, and then takes the rightmost address that is not itself a proxy. With the default empty list the header is ignored, which is right when Fleet is exposed directly.

The Compose reference deployment fixes its network to `10.77.0.0/24` and trusts it, so Caddy's address is a proxy and the client is the real browser or agent. If you change the network's subnet, change `trusted_proxies` in [`fleet.toml`](https://github.com/moyaver/fleet/blob/main/deploy/compose/fleet.toml) with it. Listing more than the proxies does harm: any address inside a listed block can write any client address into `X-Forwarded-For`, so it can escape a per-IP limit or borrow another client's. Never list `0.0.0.0/0` or `::/0` (the server refuses to start).

| Key | Default | Meaning |
|---|---|---|
| `rate_limits.ip_requests_per_minute`, `rate_limits.ip_burst` | `120`, `60` | Pre-auth requests per client IP: sustained rate, and how many may arrive at once |
| `rate_limits.credential_requests_per_minute`, `rate_limits.credential_burst` | `1200`, `300` | Agent connections per credential (WebSocket upgrades and batch requests) |
| `rate_limits.viewer_requests_per_minute`, `rate_limits.viewer_burst` | `3000`, `600` | Viewer requests per credential (a viewer token, or a browser session): REST reads and live socket upgrades. A viewer token shared by a team shares this bucket. Anonymous reads (`api.anonymous_read`, development only) have no credential and are not limited |
| `rate_limits.events_per_second`, `rate_limits.event_burst` | `1000`, `5000` | Events per second one agent socket may send on average, and at once. Beyond it the server reads that socket more slowly; no event is dropped |
| `rate_limits.max_tracked_keys` | `100000` | Clients tracked per limiter; memory stays bounded. A new client beyond this is told to retry in one second until an idle one ages out |

## Artifact storage

Screenshots, JUnit reports, logs, HTML reports and debug bundles are stored as **content-addressed blobs**: a blob's key is its project and the SHA-256 of its bytes. Uploading the same bytes twice to one project stores them once, and one project can never read or delete another project's blobs. A blob is only stored if its size and SHA-256 match what was declared; a mismatch stores nothing. The backend is chosen once, with `storage.backend`. Agents upload through the server by default (relayed: the store never needs to be reachable from runners or browsers). With `s3`, an agent that can reach the store may upload directly instead; see [Direct upload](#direct-upload) and [Artifact uploads](#artifact-uploads). The blob store is opened on first use, and the server logs a warning at start-up if it can't be opened.

### Filesystem (`local_fs`)

```toml
[storage]
backend = "local_fs"
path = "/var/lib/fleet/blobs"      # a persistent volume
```

Fine for a small install on one host. Writes are atomic (temp file, `fsync`, rename), so a crash leaves either no blob or a whole one; unfinished uploads sit in `<path>/.tmp` and are safe to delete while the server is stopped. Back the directory up together with Postgres.

### S3-compatible (`s3`)

```toml
[storage]
backend = "s3"

[storage.s3]
endpoint = "http://garage:3900"     # the S3 API; http:// is accepted on a private network
region = "garage"                   # Garage's default; the signing region
bucket = "fleet-artifacts"          # must already exist
# credentials: set FLEET_STORAGE__S3__ACCESS_KEY_ID_FILE and FLEET_STORAGE__S3__SECRET_ACCESS_KEY_FILE
```

Fleet speaks only the generic S3 API, path-style, with static credentials, so any S3-compatible store works. **Garage** is the reference (the Compose file runs it; its S3 port is `3900` and its credentials are the key pair in `secrets/s3_access_key_id` and `secrets/s3_secret_access_key` (Docker secrets, read by Fleet through `FLEET_STORAGE__S3__*_FILE`), bucket `fleet-artifacts`, region `garage`). **SeaweedFS** (Apache-2.0) is the alternative if AGPL is unacceptable for the store. Garage is a separate service reached over the network: its AGPL obligations apply to Garage only.

- The bucket needs no versioning. Give it a lifecycle rule that aborts incomplete multipart uploads after a day: an upload cut off mid-way (server restart, client gone) leaves its uploaded parts there until then. Blobs up to 8 MiB are written with one request and never leave parts behind.
- Fleet makes no request when it starts, so a wrong endpoint or key shows on the first upload, not in `check-config`.
- Staged direct uploads live under `uploads/` in the bucket. Fleet deletes them when their upload completes and sweeps leftovers after twice `artifacts.grant_ttl_seconds`; the lifecycle rule does not need to cover them.
- The same contract test suite runs against the filesystem backend and against Garage (v2.5.0, pinned by digest) in CI, so a change that breaks either fails the build.

### Direct upload

With `s3`, an agent may `PUT` its bytes straight to the object store instead of through the server's HTTP path (PLAN §7.6, ADR-0007). The server presigns each `PUT` for one staging object of that artifact, signed for the file's SHA-256 (`x-amz-checksum-sha256`), so the store itself refuses bytes that are not the declared file. The agent then calls `complete` as usual, and the server **still reads every byte** from the store to verify it before the blob is written. What direct upload saves is the server's ingress for artifact bytes (the upload itself and the copy through the server), not the server's read of them.

- **Reachability.** `storage.s3.public_endpoint` is the URL agents use, and presigned URLs name its host. Set it to a URL the agents can reach, over TLS if they are off-network. The Compose file keeps Garage internal and leaves it unset, so its agents upload through the server. An agent that cannot reach the endpoint falls back to the relayed upload after one failed `PUT` for each artifact.
- **Fallback.** A store that cannot presign (the filesystem backend, or a signing failure) answers with the relayed grant. Nothing fails because direct upload did.
- **Reuse.** A presigned URL is valid for `artifacts.grant_ttl_seconds` and can be used more than once in that time. It admits only bytes with the signed SHA-256, so reuse can stage the same file again and nothing else. The server reads the staged object once, at the first `complete`; a repeated `complete` returns the recorded outcome and never reads the object again, and it deletes any object staged after the artifact finished. A finished artifact cannot change.
- **Conditional writes.** Garage ignores `If-None-Match`, so Fleet does not rely on it.
- **Verification.** Bytes the store accepted but Fleet refuses (wrong magic bytes, a size or digest the declared values do not match, an unsafe archive) are rejected with their reason and never become a blob, exactly as relayed bytes are.

### What is checked before a blob is stored

An artifact is accepted only if its declared size and SHA-256 match, it is not larger than 512 MiB, and its content (the magic bytes, never the file name or `Content-Type`) is allowed for its class:

| Class | Accepted content |
|---|---|
| screenshot | PNG, JPEG, WebP |
| junit | XML |
| html report | an HTML file, or a zip |
| text: `maestro_log`, `commands`, `failure_hierarchy`, `device_logcat` | UTF-8 text without NUL bytes |
| bundle | zip, or `tar.zst` |

Zip and `tar.zst` archives are also read through (never unpacked to disk) and refused if they have more than 5,000 entries, expand to more than 1 GiB in total, expand more than 100:1 (judged per file in a zip, over the whole stream for `tar.zst`, above 1 MiB), contain an absolute path, `..`, a backslash, a drive letter, control characters, a symlink, hardlink, device or fifo, contain another archive, or have bytes after their end. Sizes in archive headers are ignored: only bytes actually produced by decompression count. A refused artifact is refused with its reason and nothing is kept.

## Artifact uploads

Agents upload failure screenshots, each flow's `commands.json`, the JUnit report and `maestro.log`, and two classes for failures: a failing step's UI hierarchy (`failure_hierarchy`) and a failed flow's device logcat excerpt (`device_logcat`). The HTML report is uploaded too, when the command asks Maestro for one with `--format html` or `--format html-detailed`: the file that `--output` names, or Maestro's default `report.html` in the working directory when there is no `--output`, is redacted like a log and uploaded as `html_report` (its `Content-Type` is `text/html`), provided it was written during the run. A report over 16 MiB is not uploaded, because it cannot be redacted reliably in memory (I6), and a report left by an earlier run is ignored with a `report_stale` warning. An upload that fails is journaled and never changes the run's result. The server accepts only its allowlisted classes, with the size limits above, and each run is capped at 2 GiB and 2,000 files. An upload is a request, a `PUT` of the bytes (spooled to `artifacts.spool_dir`), and a completion that verifies the bytes (see the table above) before the blob is written. Each upload grant belongs to the credential that asked, is short-lived and takes the bytes once; the same file requested again by a run is the same artifact, and bytes a project already holds for a class are not sent again. Nothing about a refused upload is kept except the reason. Spool files nobody completes are removed after twice the grant lifetime.

Retention is recorded per artifact and enforced by the retention jobs (see [Retention](#retention)).

With `s3`, the agent asks for a direct upload first and falls back to the relayed `PUT` described above when the store cannot be used (see [Direct upload](#direct-upload)); the protocol steps are in [the protocol](protocol.md#artifact-uploads).

The spool is local: with several server instances, send a given upload's `PUT` and `complete` to the same instance (or share the spool directory). A direct upload needs no spool entry before `complete`, so it is not affected by this.

## Retention

Fleet keeps what you need to debug last week's failure and see this year's trend, and lets the rest go. One server instance at a time, the **leader** (a Postgres advisory lock; the others stand by), runs the jobs below, safely beside ingest. Every job is idempotent, logs what it did at `info` (`telemetry rolled up`, `artifacts expired`, `blobs deleted`, `expired data dropped`) and at `warn` when it fails, and is retried at its next turn.

| Data | Kept | How it goes |
|---|---|---|
| Raw telemetry samples (5 s) | 7 days (`retention.raw_samples_days`) | The daily partition is detached and dropped once its whole UTC day is old enough |
| One-minute rollups | 90 days | Rows older than that are deleted |
| One-hour rollups | 1 year | Rows older than that are deleted |
| Raw agent journal, step executions | 14 days | Daily partitions are dropped. The flow outcomes, fingerprints and history stay |
| Audit log | 1 year (`retention.audit_days`) | Rows older than that are deleted |
| Artifacts | by retention class, below | Marked `expired`, so they leave the flow's list at once; the row stays as a tombstone for `retention.expired_artifact_days`, then is deleted |
| Artifact bytes (blobs) | until nothing refers to them, plus `retention.blob_grace_hours` | The object is deleted from the blob store |

**Rollups** summarise raw samples into one-minute and one-hour buckets (`n`, average and maximum). A minute is rolled up two minutes after it ends, so batches in flight settle. Samples that arrive after their minute was rolled up (an agent replaying its spool after a reconnect) re-roll just the buckets they touch. A bucket's value is the average of the readings that exist: a metric an agent never reported stays *unavailable* in the rollups and in the API, never zero. The dashboard serves ranges of up to 24 hours from raw samples, up to 30 days from one-minute rollups and longer ones from one-hour rollups, so history outlives the raw data.

**Raw partitions are never dropped before they are rolled up.** If the rollup job is behind (or was down) when a partition expires, the partition is kept, a `keeping an expired partition` warning is logged, and it is dropped by a later pass.

**Artifact classes.** Each artifact gets a retention class when it arrives, and the class decides when it expires. The defaults (seeded per project in the `retention_classes` table, shown read only on the [Administration](#administration) page; change them with SQL until the forms arrive) are:

| Class | Default | Applies to |
|---|---|---|
| `success_evidence` | 7 days | Screenshots, commands and UI hierarchies of a flow that has not failed |
| `failure_evidence` | 90 days | The same, once the flow fails (promoted when the flow finalizes, or at upload if it had already failed), and the run's Maestro log and device logcat excerpt when any of its flows failed |
| `report` | 1 year | JUnit reports and HTML reports |
| `debug_log` | 30 days | Maestro logs of runs without a failed flow |
| `video`, `bundle` | 14 days | Reserved |

A `NULL` TTL keeps a class for ever. A changed TTL applies to artifacts uploaded afterwards. Promotion never shortens an artifact's life. An artifact with `legal_hold` set is never expired. Upload requests nobody completed within a day of their grant lapsing are expired too.

**Blobs are deleted only when nothing refers to them.** Bytes are stored once per project, however many artifacts (of any run or class) name them, so an expired artifact's blob is deleted only after the *last* artifact using it has expired, and then only after the grace period. Before deleting, the collector checks again that no artifact (not even a pending upload of the same bytes) refers to the blob, and an upload that arrives while a blob is being deleted waits for the deletion to finish instead of racing it. A delete that fails (the blob store is down) is retried by the next round.

**Leftover files.** On the local filesystem backend, a crash during an upload can leave a temp file in `<storage.path>/.tmp`. Files there older than `retention.stale_upload_hours` are removed. For S3, unfinished multipart uploads are cleaned by the bucket's lifecycle rule (`AbortIncompleteMultipartUpload`); Fleet does not touch them.

Retention applies to what `fleet` reads from the database; backups made before a deletion still contain the data.

## Usercontent domain

Maestro HTML reports and raw artifacts are files from CI jobs, so Fleet treats them as hostile: it never serves them from the dashboard's origin. They are served only from a **second hostname**, `usercontent.url`, and only through **signed links** that the dashboard's API hands out after checking the caller may read artifacts (`GET /api/v1/projects/{project}/flows/{id}/artifacts`).

Use a different *registrable domain* from the dashboard (`fleet.example.com` and `fleet-usercontent.example.net`, not `usercontent.fleet.example.com`), so no cookie, `document.domain` trick or same-site request can reach the dashboard from an uploaded file. Fleet refuses a configuration where both are the same host. Get a certificate for it and point both names at the proxy:

```toml
[api]
public_url = "https://fleet.example.com"

[usercontent]
url = "https://fleet-usercontent.example.net"
# signing_key: set FLEET_USERCONTENT__SIGNING_KEY_FILE (openssl rand -base64 48)
link_ttl_seconds = 300
```

What the origin guarantees, on every response:

- A link names one artifact of one project until one moment (`/u/<project>/<artifact>/<expires>/<signature>/<file name>`; the signature is an HMAC-SHA256 under your key). A changed, expired or foreign link gets 403, 410 or 404. Rotating the key invalidates every link.
- `Content-Security-Policy: sandbox; default-src 'none'; …`: HTML reports get an opaque origin and no scripts. `X-Content-Type-Options: nosniff`, and the `Content-Type` is what the verified bytes are, not what the uploader said.
- Images and (sandboxed) HTML are `Content-Disposition: inline`; everything else is `attachment`.
- HTTP `Range` requests: a single `bytes=` range is answered `206 Partial Content` with a `Content-Range`, and a range the file cannot give is `416` with `Content-Range: bytes */<size>`. Several ranges, another unit, a malformed range or an `If-Range` header get the whole file (`200`), which HTTP allows. Every answer carries `Accept-Ranges: bytes`.
- No cookie is read or set, and nothing but `/u/…` links is served: the API, the dashboard and the agent routes answer 404 on this host.
- `Cross-Origin-Resource-Policy: cross-origin` (the dashboard embeds screenshots; the link is the access control), and `Access-Control-Allow-Origin` and `frame-ancestors` allow only `api.public_url`.

The dashboard's own policy names this origin, and only this one, in `img-src` (screenshots) and `connect-src` (the log viewer fetches `maestro.log` and `commands.json` as text); it is added from `usercontent.url`, and no other directive (scripts, styles, frames) allows it. A browser that reports a CSP violation on the flow page usually means `usercontent.url` does not match the address people reach it at.

Without `usercontent.url` artifacts are stored but cannot be opened (the flow page says so). Fleet routes by the `Host` header on its main listener, so a proxy that sends both hostnames to `fleet:8080` is enough ([`deploy/compose/Caddyfile`](https://github.com/moyaver/fleet/blob/main/deploy/compose/Caddyfile)); `usercontent.listen` serves the origin on a separate port instead.

## Shutdown

On SIGTERM the server stops accepting connections, finishes each agent's in-flight batch, sends `goaway`, writes the last telemetry checkpoint and closes sockets with 1012 (each socket waits up to one second for the agent's close frame); agents reconnect and resume where the server says. All state is in Postgres, so a restart loses nothing that was acknowledged. Give the container a stop grace period of 30 s.

## Upgrades

Upgrade the server first, then the agents. Read the [CHANGELOG](https://github.com/moyaver/fleet/blob/main/CHANGELOG.md) from your version to the new one for steps that need action before you start.

### Before you upgrade

1. **Back up** the database and the blob store, with the configuration and secrets ([Backup and restore](#backup-and-restore)). The backup is the only way back once the schema has moved; see [Rollback](#rollback).
2. **Pin the image by tag.** Set `FLEET_IMAGE` in the Compose `.env` to the release (for example `ghcr.io/moyaver/fleet:0.1.0-alpha.3`); do not follow `latest` across upgrades.

### Upgrade the server

In the Compose reference deployment, change `FLEET_IMAGE` to the new tag, then:

```bash
cd deploy/compose
docker compose pull && docker compose up -d
docker compose ps        # the fleet service is healthy when `fleet healthcheck` passes
docker compose exec fleet fleet --version   # the new version
```

On start the server applies the pending migrations (`database.migrate = "auto"`, the default), creates the next days' partitions, and serves. Migrations run under Postgres's advisory lock, so instances that start together are safe.

To run migrations yourself, set `FLEET_DATABASE__MIGRATE=manual`. The shipped `docker-compose.yml` pins `auto`, so set it in a `docker-compose.override.yml` beside it:

```yaml
services:
  fleet:
    environment:
      FLEET_DATABASE__MIGRATE: manual
```

With manual migrations, `serve` refuses an outdated schema: a plain `up -d` leaves the new container restarting. So run `docker compose pull`, then stop the old server, apply the migrations as their own step, and start the new one:

```bash
docker compose pull
docker compose stop fleet
docker compose run --rm fleet migrate
docker compose up -d
```

A server refuses a database whose schema is newer than its own, so an old image cannot run against a migrated database.

### Migrations

- Every migration is a plain `NNNN_name.sql` file, and every one runs in its own transaction. A failed migration leaves the schema as it was.
- **Migrations are forward-only.** There are no down migrations. The schema never moves back: rolling back is a restore ([Rollback](#rollback)).
- **Applied migrations are never edited.** sqlx stores a checksum of each applied migration and the server refuses to start when an applied file no longer matches it. A fix is a new migration.
- Each release's migrations are the previous release's plus new files, and an upgrade applies the new ones in order. The migrations of each tag are listed in `migrations/` at that tag: upgrading from `v0.1.0-alpha.1` to `v0.1.0-alpha.3` applies `0015` to `0017`.

### Agents and server versions

- Agents and the server negotiate one wire protocol, `PROTOCOL_VERSION` (`1` in every release so far). An agent's `hello` names the range it speaks, and the server accepts the connection only when its own version is in that range. Otherwise it answers `reject` with `unsupported_protocol`, which is fatal for that run: the agent stops reporting to Fleet, and Maestro is unaffected.
- Within one protocol version, changes are additive optional fields. Neither side rejects a field it does not know, and a frame an agent cannot decode is dropped, so a newer agent and an older server (or the reverse) keep working on the same version.
- A change that is not additive bumps the protocol version. Then the server must be upgraded before the agents that need the new version, which is why the server goes first. Agents still on the old version are refused until they are upgraded.

### Rollback

The schema does not move back, and the previous release refuses the upgraded schema (it does not know the migrations the upgrade added, so it exits at start with "the database schema is newer than this build"). Rolling back is therefore a restore:

1. Stop the new server: `docker compose stop fleet`.
2. Set `FLEET_IMAGE` back to the previous tag and restore the backup taken before the upgrade, as in [Restore](#restore): a new stack under its own Compose project name, so the upgraded volumes stay intact until you have checked the rollback.
3. Start the previous image and check the runs you know are there.

Everything recorded after the backup is lost from the database. Agents hold unacknowledged events in their spool and resend what the restored server asks for (its `committed_through`), for as long as the spool still has it. Agents can stay on their version: while the protocol version is unchanged they work with either server.

### Rehearse the upgrade

Operators can rehearse on a copy: restore the latest backup into a stack under its own Compose project name (see [Restore](#restore)), set `FLEET_IMAGE` there to the new tag, and check that the server starts, that `SELECT count(*) FROM artifacts` matches the source, and that a run you know lists in the dashboard.

Maintainers can rehearse the migrations without a deployment: `cargo xtask upgrade-rehearsal` exports the `migrations/` of every release tag from git (fetch the tags first, `git fetch --tags`), seeds rows under each release's schema, migrates to the current tree, and checks that every row is unchanged, that the current read paths return the seeded runs, flows and artifacts, and that the release's own migrator refuses the upgraded schema. It takes a few seconds once Postgres 18 is up (`DATABASE_URL`, or a throwaway container). CI runs it after `cargo xtask ci` in the main job, on the job's Postgres, and its checkout fetches every tag (`fetch-depth: 0`). The test is `crates/fleet-store/tests/upgrade.rs`.

### Verified

On 2026-11-08 the Compose reference deployment went from `v0.1.0-alpha.1` to `v0.1.0-alpha.3` with the steps above: a backup, a project, an API key and a run were created, and after the upgrade migrations `0015` to `0017` were applied, the data was still there, the server reported `0.1.0-alpha.3` and new agents were accepted. The manual-migration path was checked on a second stack: the outdated schema is refused, and the migrate-then-start order works. The older server refuses the upgraded schema.

## Backup and restore

Fleet keeps its state in two places. **Postgres 18** holds everything except artifact bytes: runs, flows, telemetry rollups, the audit log, the artifact and blob index, and API keys and sessions (stored as hashes). The **blob store** holds the bytes of artifacts (screenshots, JUnit reports, logs, HTML reports, bundles), content-addressed by project and SHA-256. A backup is both, plus the configuration and secrets needed to start the server again.

Backups hold user names, audit events and every artifact, so encrypt them at rest and restrict who can read them. [Retention](#retention) removes data from the live database and blob store only: a backup keeps it until you delete the backup, so keep backups no longer than the retention you promise.

### What to back up

| What | Compose deployment | How |
|---|---|---|
| Database | `postgres`, database `fleet` | `pg_dump -Fc` (custom format) |
| Blob store | Garage bucket `fleet-artifacts` | copy the bucket through the S3 API |
| Blob store, `local_fs` | the `storage.path` directory | copy the directory |
| Configuration | `fleet.toml`, `Caddyfile`, `.env`, `secrets/` | copy, and keep the secret values in your secret store |

Do not back up `artifacts.spool_dir` (the `fleet_spool` volume). It holds uploads waiting for verification; an upload cut off by the backup is abandoned and its artifact expires.

The secrets a restore needs:

- **`secrets/s3_access_key_id` and `secrets/s3_secret_access_key`** (Fleet's S3 credentials, the key the bucket is created with, also in `secrets/garage.env`) and **`secrets/garage_rpc_secret`**. Without them the copied bucket cannot be read.
- **`secrets/postgres_password` and `secrets/database_url`**: the restored Postgres takes its password from `postgres_password` when its volume is first created, and `database_url` must match it.
- **`secrets/viewer_token`**: the bearer token for the REST API and CI scripts. Keep it, or every script that uses it breaks.
- **`secrets/usercontent_key`**: signs links to uploaded files. A new key works but invalidates links in flight (they last `usercontent.link_ttl_seconds`, 5 minutes by default).
- **The `[login]` client secret** (`FLEET_LOGIN__CLIENT_SECRET_FILE`), if you use browser sign-in.

API keys and browser sessions are rows in Postgres, so the database dump carries them: there is nothing else to back up for them.

### Order: database first, then blobs

1. **Dump the database.** `pg_dump` reads one consistent snapshot while the server runs.
2. **Copy the blob store**, and finish within `retention.blob_grace_hours` (24 by default) of starting step 1.

The order follows from how the retention jobs work ([`gc.rs`](https://github.com/moyaver/fleet/blob/main/crates/fleet-store/src/gc.rs), [`retention.rs`](https://github.com/moyaver/fleet/blob/main/crates/fleet-server/src/retention.rs)):

- An artifact becomes `available` only after its bytes are stored.
- The collector deletes a blob only when no live artifact refers to it and `retention.blob_grace_hours` have passed since the last one expired. So a blob that an `available` artifact in the dump refers to is still in the bucket for that long after the dump, and a copy that finishes in that window has every blob those artifacts need. Expired artifacts in the dump may have lost their bytes; they are tombstones and are never served.
- Blobs written after the dump are copied too. After a restore nothing refers to them, and nothing deletes objects the database does not know about, so they stay and take space. They break nothing; delete them by hand if the space matters.

The reverse order is unsafe: an artifact that becomes `available` between the bucket copy and the dump would refer to bytes the copy does not have.

### Take a backup

Run this in `deploy/compose`. The server keeps running; the Garage container is reached from a tool container that shares its network namespace, so nothing is published on the host.

```bash
cd deploy/compose
export AWS_ACCESS_KEY_ID="$(cat secrets/s3_access_key_id)" AWS_SECRET_ACCESS_KEY="$(cat secrets/s3_secret_access_key)"
BACKUP=/var/backups/fleet/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$BACKUP/blobs"

# 1. Database: one consistent snapshot
docker compose exec -T postgres pg_dump -Fc -U fleet -d fleet > "$BACKUP/fleet.dump"

# 2. Blobs: the bucket, through Garage's S3 API (s3 sync copies only what is new or changed)
docker run --rm --network container:"$(docker compose ps -q garage)" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION=garage \
  -e AWS_REQUEST_CHECKSUM_CALCULATION=when_required -e AWS_RESPONSE_CHECKSUM_VALIDATION=when_required \
  -v "$BACKUP/blobs:/backup/blobs" amazon/aws-cli:latest \
  --endpoint-url http://127.0.0.1:3900 s3 sync s3://fleet-artifacts /backup/blobs
```

Then copy `$BACKUP` and the configuration off the host. For `local_fs`, dump the database as above and copy `storage.path` (skipping `.tmp`): blobs are written by an atomic rename, so copying a live directory is safe in the same order.

### Restore

Restore into a new stack under its own Compose project name (`-p`), so it gets its own volumes. Do not restore over a running server: `pg_restore --clean` replaces the database objects underneath it. Use the same `.env` and `secrets/` as the backup, and the image that wrote the backup or a newer one, since migrations only move forward.

```bash
cd deploy/compose
export AWS_ACCESS_KEY_ID="$(cat secrets/s3_access_key_id)" AWS_SECRET_ACCESS_KEY="$(cat secrets/s3_secret_access_key)"
BACKUP=/var/backups/fleet/20260101T000000Z        # the backup to restore

docker compose -p fleet-restore up -d --wait postgres garage
docker compose -p fleet-restore exec -T postgres \
  pg_restore -U fleet -d fleet --no-owner --clean --if-exists < "$BACKUP/fleet.dump"
docker run --rm --network container:"$(docker compose -p fleet-restore ps -q garage)" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION=garage \
  -e AWS_REQUEST_CHECKSUM_CALCULATION=when_required -e AWS_RESPONSE_CHECKSUM_VALIDATION=when_required \
  -v "$BACKUP/blobs:/backup/blobs" amazon/aws-cli:latest \
  --endpoint-url http://127.0.0.1:3900 s3 sync /backup/blobs s3://fleet-artifacts
docker compose -p fleet-restore up -d --wait fleet
```

The restored stack has no proxy in this sequence. Start one when the restored stack is the live one, on its own host or with the old stack stopped, since both publish ports 80 and 443. Point the agents' `FLEET_SERVER` and the DNS name at it.

Runs that were in progress at the dump come back `active`. Once their agents stop reporting they are detached, and they are abandoned after `liveness.resume_grace_ms` unless an agent resumes them; their open flows then become `unknown`, and so do their open steps (each shows Inferred), never success.

### Verify a restore

- Run [`deploy/compose/backup-restore-check.sh`](https://github.com/moyaver/fleet/blob/main/deploy/compose/backup-restore-check.sh). It runs the whole sequence above against throwaway stacks (`fleet-backup-src` and `fleet-backup-dst`, or `SRC_PROJECT` and `DST_PROJECT`) and checks that a row written before the backup is in the restored database, that the REST API lists the project, and that the bucket inventory matches the source. It writes an API key and a canary object to the source stack, so do not point it at production. It removes both stacks and their volumes when it finishes.
- On a restored live stack, compare the state with what you recorded before the backup: `docker compose exec -T postgres psql -U fleet -d fleet -c 'SELECT count(*) FROM artifacts'`, and open a run you know. `curl -H "Authorization: Bearer $(cat secrets/viewer_token)" https://<FLEET_DOMAIN>/api/v1/projects/<slug>/attempts` lists it, and its flow page in the dashboard shows the screenshots, which come from the blob store.

## Load testing

`fleet-loadgen` drives simulated agents through the real protocol (`--agents`, `--devices`, `--storm` for a reconnect storm, `--duplicates` for resent and reordered batches) and fails unless every event is acked. Against a deployment behind TLS use `wss://` and point `SSL_CERT_FILE` at the proxy's CA if it is not publicly trusted; see [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md#load-test-a-deployment).
