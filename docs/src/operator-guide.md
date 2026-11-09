# Operator guide

## Deploy

The reference deployment is Docker Compose. It runs Caddy (TLS, WebSocket-friendly), the Fleet server, Postgres 18 and Garage (S3-compatible, the artifact store). Step-by-step: [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md).

The image is built from [`deploy/docker/Dockerfile`](https://github.com/moyaver/fleet/blob/main/deploy/docker/Dockerfile). It uses cargo-chef, a distroless base and a non-root user. Each release tag publishes it to `ghcr.io/moyaver/fleet`, tagged with the version. Stable releases also get `latest`.

Pin a version with `FLEET_IMAGE` in the Compose `.env`. For a single free cloud VM, see [Try Fleet on a free VM](free-vm.md).

## The `fleet` binary

| Command | Does |
|---|---|
| `fleet serve` | Serves the dashboard, the agent protocol and the API |
| `fleet migrate` | Applies database migrations and exits |
| `fleet check-config` | Validates the configuration and prints it, secrets redacted |
| `fleet healthcheck` | Exits 0 if the local server is ready (`/readyz`); for Docker `HEALTHCHECK` |
| `fleet api-key create --project <slug> --name <n> [--expires-in-days <d>]` | Prints a new API key once (`mf_live_…`); only its hash is stored. The project is created if it does not exist. The key expires after `d` days, or after `api.api_key_ttl_days` (90) when you omit the flag |
| `fleet api-key list --project <slug>` | Prints a header row, then each key's ID, name, prefix, expiry, revocation and last use, tab-separated. No secret is shown |
| `fleet api-key revoke --id <id>` | Revokes a key at once. Every agent token minted from it stops too. Works on keys made before expiry existed. Revoking a revoked key is fine |
| `fleet api-key expire --id <id> --in-days <d>` | Sets when a key stops working, `d` days from now. `0` ends it now. Use it for keys made before expiry existed, or to shorten a key. Refused for a revoked key |

`fleet --version` prints the server version.

## Configuration

Fleet reads its settings in three layers. Defaults come first. Then a TOML file (`--config` or `FLEET_CONFIG`). Then environment variables, which override the file. `FLEET_<key>` sets a value, where `__` separates sections (`FLEET_DATABASE__URL`). `FLEET_<key>_FILE` reads the value from a file, as Docker and Kubernetes secrets do. Both forms are read in the same environment layer. If you set `FLEET_<key>` and `FLEET_<key>_FILE` for the same key, the code does not define which one applies: it depends on the order in which the process lists its variables. Set only one of them. Errors name the variable or key at fault.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Address to listen on. The image sets `0.0.0.0:8080` |
| `trusted_proxies` | `[]` | CIDR blocks of reverse proxies. Their `X-Forwarded-For` names the client for the limits below. See [Client addresses and limits](#client-addresses-and-limits) |
| `rate_limits.*` | see below | Per-IP and per-credential request limits, and the per-socket event rate |
| `database.url` | — | `postgres://…`. Set it with `FLEET_DATABASE__URL_FILE` in Docker |
| `database.migrate` | `auto` | `auto` migrates on start. `manual` refuses an outdated or newer schema |
| `ingest.heartbeat_interval_ms` | `10000` | Agent heartbeat cadence, sent to agents in `welcome` |
| `ingest.ping_interval_ms` | `20000` | Server pings, below common 60 s proxy idle cuts |
| `ingest.idle_timeout_ms` | `45000` | A silent agent socket is closed, and its run detached |
| `ingest.telemetry_flush_ms` | `15000` | Telemetry checkpoint (10–30 s), then `telemetry_ack` |
| `ingest.shutdown_grace_ms` | `15000` | Drain time for sockets on SIGTERM |
| `ingest.sample_interval_ms` | — | How often agents sample host and process metrics while the host is calm, from 1 000 to 300 000 ms. Unset, agents sample every 5 s. Sent to agents in `welcome`. A project may override it on the [Administration](#administration) page |
| `ingest.pressure_sample_interval_ms` | — | The same, while the host is under pressure, from 1 000 to 300 000 ms. Unset, agents sample every 15 s |
| `ingest.max_frame_bytes` | `262144` | Largest WebSocket frame and batch request accepted (256 KiB, research 06 T12). Agents fill each frame up to this limit and never send more. An agent event over 192 KiB is stored as a gap instead |
| `liveness.stale_after_ms` | `25000` | A connected agent silent this long shows `stale` in the dashboard. Display only |
| `liveness.resume_grace_ms` | `900000` | A detached run is abandoned after this long. Its open flows become `unknown`, and its open steps become `unknown` and Inferred |
| `api.viewer_tokens` | — | Bearer tokens that may read every project, for scripts and tests. People sign in with `[login]` |
| `api.api_key_ttl_days` | `90` | Days until a new API key expires, when `fleet api-key create` gets no `--expires-in-days`. Keys made before this setting have no expiry. End their life with *Expire* on the [Administration](#administration) page, or with `fleet api-key expire` |
| `api.public_url` | — | The dashboard's origin. Browser sockets from other origins are refused. The sign-in redirect URI is built from it. Required with `[login]` |
| `login.*` | — | Browser sign-in with your identity provider. See [Browser sign-in](#browser-sign-in) |
| `storage.backend` | `local_fs` | Where artifacts live: `local_fs` or `s3`. See [Artifact storage](#artifact-storage) |
| `storage.path` | `/var/lib/fleet/blobs` | Blob directory for `local_fs` |
| `storage.s3.*` | — | Endpoint, region, bucket and credentials for `s3` |
| `storage.s3.public_endpoint` | — | Base URL that agents reach the bucket at, for direct uploads. Unset uses `endpoint`. See [Direct upload](#direct-upload) |
| `artifacts.spool_dir` | `/var/lib/fleet/spool` | Where uploaded bytes wait for verification. A local disk: give each server instance its own directory |
| `artifacts.grant_ttl_seconds` | `600` | How long an upload grant lasts |
| `artifacts.max_run_bytes`, `artifacts.max_run_files` | 2 GiB, 2000 | What one agent run may store |
| `artifacts.max_project_bytes_per_day`, `artifacts.max_project_files_per_day` | 20 GiB, 20000 | What one project may declare in any 24 hours, across all its runs. A refused upload says `project_quota_exceeded` |
| `artifacts.max_bytes.<class>` | per class, see Meaning | Size limit per class, in bytes. A file over its class's limit is refused before its bytes are sent. Defaults: `failure_screenshot` 20 MiB, `commands` 16 MiB, `failure_hierarchy` 4 MiB, `device_logcat` 512 KiB, `junit` 32 MiB, `runner_log` 64 MiB, `html_report` 128 MiB, `user_screenshot` 20 MiB, `user_recording` 100 MiB. `failure_hierarchy` is a failing step's UI hierarchy. `device_logcat` is a failed flow's logcat excerpt. `user_screenshot` and `user_recording` are Maestro's `takeScreenshot` image and `startRecording` video. See [Artifact uploads](#artifact-uploads) |
| `retention.raw_samples_days` | `7` | Raw 5 s telemetry samples. See [Retention](#retention). `0` keeps data for ever, as for every `retention.*_days` |
| `retention.rollup_1m_days`, `retention.rollup_1h_days` | `90`, `365` | One-minute and one-hour rollups |
| `retention.journal_days`, `retention.steps_days` | `14`, `14` | The raw agent journal and the step executions |
| `retention.audit_days` | `365` | The audit log |
| `retention.blob_grace_hours` | `24` | How long a blob that no artifact refers to is kept before it is deleted |
| `retention.expired_artifact_days` | `30` | How long an expired artifact's row stays as a tombstone for history |
| `retention.stale_upload_hours` | `24` | Temp files of crashed uploads older than this are removed. Local store only |
| `retention.rollup_interval_seconds`, `retention.gc_interval_minutes` | `60`, `10` | How often rollups run, and how often artifacts expire and blobs are collected |
| `usercontent.url` | — | The usercontent origin. See [Usercontent domain](#usercontent-domain) |
| `usercontent.signing_key` | — | HMAC key for signed links, at least 32 bytes. Set it with `FLEET_USERCONTENT__SIGNING_KEY_FILE` |
| `usercontent.link_ttl_seconds` | `300` | How long a signed link works (1 to 300) |
| `usercontent.listen` | — | Also serves the usercontent origin on its own address |
| `oidc.issuers` | GitHub, audience `fleet` | CI OIDC issuers accepted by `POST /api/v1/agent/token`. Each issuer has an `audience` and optional `jwks_hosts`. `jwks_hosts` lists the hosts, besides the issuer's own, that its discovery document's `jwks_uri` may name. Any other host is refused and never fetched (research 06 T14) |
| `projects[].trust_policies[]` | — | Who may publish with an OIDC token: repository and owner IDs, trusted refs, PR policy |

## Browser sign-in

People sign in to the dashboard through your identity provider. Fleet uses OpenID Connect, with the authorization code flow and PKCE. Register Fleet with the redirect URI `<api.public_url>/api/v1/auth/callback`. Fleet is a confidential client when `client_secret` is set. It is a public, PKCE-only client when it is not.

The provider must sign ID tokens with RS256, and it must send the user's groups in the ID token. Entra ID, Okta, Keycloak and Google sign with RS256 by default.

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

- **Roles come from groups, at sign-in.** A user's effective role on a project is the highest role their groups grant for that project. A group mapping with no project grants its role on every project. A role can also be bound to a person directly. A user whose groups grant nothing is refused. So is an ID token that carries none of the `group_claims`. Fleet never guesses.
- **Entra ID omits `groups` for users in more than 200 groups.** Use app roles (`roles`) there.
- **A removed group membership takes effect at the next sign-in.** That is within `session_ttl_minutes`.
- **Sessions end when they go idle.** A session ends after `session_idle_minutes` (default 60) without a request. Each request renews it. An open live socket does not renew it. Its recheck ends the socket once the session has gone idle (research 06 T21).
- **Sessions are an opaque cookie.** The cookie is `__Host-fleet_session`, with `HttpOnly`, `Secure` and `SameSite=Lax`. Postgres stores each session by hash.
- **Requests that change data need a CSRF token.** Send the session's token in `X-CSRF-Token`, from `GET /api/v1/auth/session`. When the browser sends an `Origin` header, it must be the dashboard's own.
- **Serve the dashboard over HTTPS.** The session cookie is `Secure` and has the `__Host-` prefix. Browsers accept both only from a secure origin, so sign-in fails over plain `http://`. Browsers treat `localhost` and `127.0.0.1` as secure, which is how the end-to-end tests run. Put TLS in front of Fleet. The Compose file's Caddy does this. Set `api.public_url` to the `https://` address people use.
- A project the user has no role on looks exactly like a project that does not exist.

## Administration

Global administrators see **Administration** in the navigation, at `/admin`. They also see the audit history, at `/admin/audit`. Everyone else is refused on those URLs and in the API. That includes a project's own admin, a viewer token and anonymous readers. Grant the role with `[[login.group_roles]]` and no `project`, or bind it to a person as a role for every project.

The page changes projects, publisher trust, API keys, role mappings, and each project's retention, artifact policy and metric collection. It uses forms. Each change writes an audit event in the same transaction as the change. The event records who made the change, what changed, and the values before and after where they were recorded. A refused change changes nothing and writes no event.

The forms post to `/api/v1/admin/…` with the session's CSRF token. A request from another site is refused. The same changes can be made through that API with JSON. Use `PUT` and `DELETE` on the rows, as well as `POST`.

The page shows one section per area:

| Section | Where the data lives | Changed by |
|---|---|---|
| Projects (onboarding) | `projects`, with each project's working API keys and trust policies | the page: create a project (a new slug only), and rename a project the page created. A project from `projects[]` cannot be renamed on the page (see Locks below). Its name comes from the configuration at each start, so change `name` there |
| Publisher trust | `publisher_trust_policies` | `projects[].trust_policies` (configured, marked *from config*); the page: add, edit, revoke and remove the rows it added |
| API keys | `api_keys`: name, prefix, trust level, expiry, revocation, last use. The secret and its hash are never shown | the page: create (the secret is shown once, on the page the form answers with), expire, revoke. `fleet api-key create`, `revoke` and `expire` do the same from the command line |
| Role mappings | `role_bindings` (people) and `group_mappings` (identity-provider groups) | `[[login.group_roles]]` (configured, marked *from config*); the page: give or remove a person's role, and add, change and remove the groups it maps |
| Retention | `retention_classes`: each project's window for each artifact class, seeded with the defaults in [Retention](#retention); and the `retention.*` windows of the configuration | the page, per project and class: days kept, 0 for ever. A changed window applies to artifacts that become available afterwards. The `retention.*` windows are changed in the configuration file, then a restart |
| Artifact policy | The `storage`, `artifacts` and `usercontent` settings of the running configuration: each class's largest file (`artifacts.max_bytes.<class>`) and the daily quota (`artifacts.max_project_bytes_per_day`, `artifacts.max_project_files_per_day`) | the page, per project: accept or turn off each artifact class, set its largest file, and set the daily bytes and files. Each value can only be lower than the configured one. `storage`, `usercontent` and the other `artifacts` settings: the configuration file, then a restart |
| Metric collection | `ingest.sample_interval_ms` and `ingest.pressure_sample_interval_ms` (unset: 5 s, and 15 s under host pressure), and the `ingest` checkpoint and heartbeat and the `retention` windows of the configuration | the page, per project: the sampling interval while the host is calm and under pressure, from 1 to 300 seconds. An agent takes a new interval at its next welcome. The checkpoint, heartbeat and retention windows are changed in the configuration file, then a restart |
| Agent compatibility | `agent_runs`: each agent version and protocol seen, with its open runs. Compatible means the protocol is the one this server speaks | agents are upgraded outside Fleet |
| Ingestion health | `ingest_rejections` (refused events), `ingest_gaps` (losses the agents reported), and the open runs with how long since the server last heard from them and how far behind their newest event was | |
| Audit history | `audit_events`, newest first, 50 at a time. Each event shows who (a person, an API key, or the system), what, when, and the values it changed where one was recorded (`before` and `after`, or the `from` and `to` of a triage change) | recorded by Fleet; kept for `retention.audit_days` |

A row marked **from config** comes from the configuration file. The start-up sync replaces those rows, and the page does not change them. To change one, edit the configuration, then restart.

A row added on the page is stored in the database. It survives restarts, and you can edit or remove it on the page. If the configuration adds a trust policy or a group mapping that a page row already has (the same name, or the same group on the same scope), the server does not start. It names the row. Remove the page row, or the configured entry. The page refuses the reverse, and says which one to change.

**Per-project values.** The configuration sets the server-wide value of the artifact size limits (`artifacts.max_bytes.<class>`), the daily quotas (`artifacts.max_project_bytes_per_day`, `artifacts.max_project_files_per_day`) and the metric sampling intervals (`ingest.sample_interval_ms`, `ingest.pressure_sample_interval_ms`). A project's value on the page replaces the server-wide value for that project only. A blank field keeps the value in force, and the page shows that value.

An artifact override can only tighten the server-wide size cap. A project's largest file for a class, and its daily bytes and files, cannot exceed the configured value. The label of each field says so. The server refuses a higher value and changes nothing. Artifact retention windows have no configured value. Each project starts from the defaults in [Retention](#retention). The page sets each project's window per class, up to 3650 days (0 keeps for ever).

**Locks.** A project created by the configuration (`projects[]`) cannot be renamed on the page. Its rename form is replaced by the note *from config*. The API answers `409` with a reason that names the configuration. Its name comes from the configuration at each start. A project created on the page is renamed on the page, and its slug never changes.

Some rules the page keeps:

- A person has one role per project. To change it, remove the role and give the new one. Their open sessions end, so the new role applies at their next request.
- A group mapping takes effect when each member next signs in, because roles are resolved then.
- The last person with a global administrator role cannot be removed. The page refuses it. A group mapped to `admin` does not count, because Fleet cannot count a group's members.
- Revoking a key or a trust policy ends the agent tokens minted from it at once. Removing a trust policy ends them too.
- Expiry is in days from now. `0` ends a key now.

Values from the configuration are shown with their key and marked *from config*. An operator can see what the running server uses. The rows above say which values the page changes, and which ones the configuration file changes.

## Client addresses and limits

Requests before sign-in are limited per client IP. These are `POST /api/v1/agent/token`, browser sign-in and its callback. Each agent credential is limited on its WebSocket upgrades and batch requests. Each viewer credential (a configured viewer token, or a browser session) is limited on REST reads and live socket upgrades.

A refused request gets `429 Too Many Requests` with `Retry-After` in seconds. The agent waits that long and keeps spooling. A limit slows a run's upload, and never fails the run. The dashboard waits and keeps the data it has on screen.

`trusted_proxies` lists the peers that are proxies. Behind a reverse proxy, the peer is the proxy, so the client is named by `X-Forwarded-For`. Fleet reads that header only when the peer is inside a listed block. It then takes the rightmost address that is not itself a proxy. With the default empty list, the header is ignored. That is right when Fleet is exposed directly.

The Compose reference deployment fixes its network to `10.77.0.0/24` and trusts it. Caddy's address is therefore a proxy, and the client is the real browser or agent. If you change the network's subnet, change `trusted_proxies` in [`fleet.toml`](https://github.com/moyaver/fleet/blob/main/deploy/compose/fleet.toml) to match.

Listing more than the proxies does harm. Any address inside a listed block can write any client address into `X-Forwarded-For`. It can then escape a per-IP limit, or borrow another client's. Never list `0.0.0.0/0` or `::/0`. The server refuses to start with them.

| Key | Default | Meaning |
|---|---|---|
| `rate_limits.ip_requests_per_minute`, `rate_limits.ip_burst` | `120`, `60` | Pre-auth requests per client IP: the sustained rate, and how many may arrive at once |
| `rate_limits.credential_requests_per_minute`, `rate_limits.credential_burst` | `1200`, `300` | Agent connections per credential (WebSocket upgrades and batch requests) |
| `rate_limits.viewer_requests_per_minute`, `rate_limits.viewer_burst` | `3000`, `600` | Viewer requests per credential (a viewer token, or a browser session): REST reads and live socket upgrades. A viewer token shared by a team shares this bucket. Anonymous reads (`api.anonymous_read`, development only) have no credential and are not limited |
| `rate_limits.events_per_second`, `rate_limits.event_burst` | `1000`, `5000` | Events per second one agent socket may send on average, and at once. Beyond that, the server reads the socket more slowly. No event is dropped |
| `rate_limits.max_tracked_keys` | `100000` | Clients tracked per limiter, which keeps memory bounded. A new client beyond this limit is told to retry in one second, until an idle one ages out |

## A busy database

The server stores a batch in one transaction. It uses a pool of `database.max_connections` connections (20 by default). A batch that waits longer than the pool's 5-second acquire timeout is not stored. The agent gets a retryable `overloaded` error. It keeps its connection, waits, and resends the batch (see [the protocol reference](protocol.md#session)).

Nothing is lost, and no run fails because of it. But each refusal is a batch the agents must send again. A steady stream of these refusals means the database cannot keep up with the agents.

The server writes one line for these refusals, at most once every 10 seconds. The line gives the number refused since the last line:

```
WARN the database is busy: refusing batches for the agents to resend refused=<n>
```

What to do: if Postgres has connections to spare (its `max_connections`), raise `database.max_connections`. Otherwise, give the database more CPU, memory or disk, or run fewer agents against it. Other database failures are not retried this way. The agent's connection closes, and the server logs an `error` line.

## Artifact storage

Screenshots, recordings, JUnit reports, logs and HTML reports are stored as **content-addressed blobs**. A blob's key is its project and the SHA-256 of its bytes. Uploading the same bytes twice to one project stores them once. One project can never read or delete another project's blobs.

A blob is stored only if its size and SHA-256 match what was declared. A mismatch stores nothing. The backend is chosen once, with `storage.backend`.

Agents upload through the server by default. The store never needs to be reachable from runners or browsers. With `s3`, an agent that can reach the store may upload directly instead. See [Direct upload](#direct-upload) and [Artifact uploads](#artifact-uploads).

The blob store opens on first use. The server logs a warning at start-up if it cannot open the store.

### Filesystem (`local_fs`)

```toml
[storage]
backend = "local_fs"
path = "/var/lib/fleet/blobs"      # a persistent volume
```

Fine for a small install on one host. Writes are atomic: a temp file, `fsync`, then rename. A crash leaves either no blob or a whole one. Unfinished uploads sit in `<path>/.tmp`, and they are safe to delete while the server is stopped. Back up the directory together with Postgres.

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

Fleet speaks only the generic S3 API. It uses path-style addressing and static credentials, so any S3-compatible store works.

**Garage** is the reference. The Compose file runs it. Its S3 port is `3900`. Its credentials are the key pair in `secrets/s3_access_key_id` and `secrets/s3_secret_access_key`, which Fleet reads through `FLEET_STORAGE__S3__*_FILE`. The bucket is `fleet-artifacts`, and the region is `garage`.

**SeaweedFS** (Apache-2.0) is the alternative if AGPL is unacceptable for the store. Garage runs as a separate service, reached over the network. Its AGPL obligations apply to Garage only.

- Leave versioning off the bucket. Add a lifecycle rule that aborts incomplete multipart uploads after one day. An upload cut off mid-way leaves its uploaded parts until then. Blobs up to 8 MiB are written with one request, and never leave parts behind.
- Fleet makes no request at start-up. A wrong endpoint or key shows on the first upload, not in `check-config`.
- Staged direct uploads live under `uploads/` in the bucket. Fleet deletes them when their upload completes. It sweeps leftovers after twice `artifacts.grant_ttl_seconds`, so the lifecycle rule does not need to cover them.
- The same contract test suite runs against the filesystem backend, and against Garage (v2.5.0, pinned by digest), in CI. A change that breaks either one fails the build.

### Direct upload

With `s3`, an agent may `PUT` its bytes straight to the object store. This bypasses the server's HTTP path (PLAN §7.6, ADR-0007). The server presigns each `PUT` for one staging object of that artifact. The URL is signed for the file's SHA-256 (`x-amz-checksum-sha256`), so the store itself refuses bytes that are not the declared file.

The agent then calls `complete` as usual. The server **still reads every byte** from the store, and verifies it before the blob is written. Direct upload saves the server's ingress for artifact bytes: the upload itself, and the copy through the server. It does not save the server's read of them.

- **Reachability.** `storage.s3.public_endpoint` is the URL agents use, and presigned URLs name its host. Set it to a URL the agents can reach, over TLS if they are off-network. The Compose file keeps Garage internal and leaves this unset, so its agents upload through the server. An agent that cannot reach the endpoint falls back to the relayed upload after one failed `PUT` for each artifact.
- **Fallback.** A store that cannot presign (the filesystem backend, or a signing failure) answers with the relayed grant. Nothing fails because direct upload did.
- **Reuse.** A presigned URL is valid for `artifacts.grant_ttl_seconds`, and it can be used more than once in that time. It admits only bytes with the signed SHA-256. So reuse can stage the same file again, and nothing else. The server reads the staged object once, at the first `complete`. A repeated `complete` returns the recorded outcome and never reads the object again. It also deletes any object staged after the artifact finished. A finished artifact cannot change.
- **Conditional writes.** Garage ignores `If-None-Match`, so Fleet does not rely on it.
- **Verification.** Bytes the store accepted, but Fleet refuses, are rejected with their reason. Examples are wrong magic bytes, a size or digest that does not match the declared values, and an unsafe archive. Such bytes never become a blob. Relayed bytes get the same treatment.

### What is checked before a blob is stored

An artifact is accepted only if its declared size and SHA-256 match. It must not be larger than 512 MiB. Its content must be allowed for its class. The check reads the magic bytes, never the file name or `Content-Type`.

| Class | Accepted content |
|---|---|
| `failure_screenshot` | PNG, JPEG, WebP |
| `user_screenshot` | PNG |
| `user_recording` | MP4 |
| `junit` | XML |
| `html_report` | an HTML file, or a zip |
| text: `runner_log`, `commands`, `failure_hierarchy`, `device_logcat` | UTF-8 text without NUL bytes |

The server also reads zip archives through, without unpacking them to disk. It refuses an archive that has more than 5,000 entries, or that expands to more than 1 GiB in total. It refuses an entry that expands more than 100 to 1. The ratio applies per file, and only above 1 MiB.

It also refuses an archive with any of these: an absolute path, `..`, a backslash, a drive letter, control characters, a symlink, a hardlink, a device or fifo, another archive, or bytes after its end. Sizes in archive headers are ignored. Only bytes that decompression actually produces count. A refused artifact is refused with its reason, and nothing is kept.

## Artifact uploads

Agents upload these artifacts for each run:

- Failure screenshots (`failure_screenshot`), each flow's `commands.json`, the JUnit report, and the runner's log (`maestro.log`, class `runner_log`).
- A failing step's UI hierarchy (`failure_hierarchy`), and a failed flow's device logcat excerpt (`device_logcat`).

Maestro's own screenshots and recordings are uploaded only when the user-media switch is on. Turn it on with `--upload-user-media` on `fleet-agent run`, or with `FLEET_UPLOAD_USER_MEDIA=1`. Only a failed flow's files are uploaded, each on the step that made it. `takeScreenshot` images go as `user_screenshot`. `startRecording` videos go as `user_recording`.

These files are binary, so the agent never redacts them (I6). The switch is an operator's decision. A project that must keep them off the server turns the class off on the Administration page. Each class has its own size cap above, its own content type, and the `user_media` retention class.

The HTML report is uploaded too, when the command asks Maestro for one with `--format html` or `--format html-detailed`. The report is the file that `--output` names. Without `--output`, it is Maestro's default `report.html` in the working directory. The agent redacts the report like a log and uploads it as `html_report`, with `Content-Type` `text/html`. It uploads the report only if the file was written during the run. A report over 16 MiB is not uploaded, because it cannot be redacted reliably in memory (I6). A report left by an earlier run is ignored, with a `report_stale` warning.

An upload that fails is journaled. It never changes the run's result. The server accepts only its allowlisted classes, within the size limits above. Each run is capped at 2 GiB and 2,000 files.

An upload has three steps: a request, a `PUT` of the bytes (spooled to `artifacts.spool_dir`), and a completion. The completion verifies the bytes (see the table above) before the blob is written. Each upload grant belongs to the credential that asked for it. It is short-lived, and it takes the bytes once. The same file requested again by a run is the same artifact. Bytes a project already holds for a class are not sent again. A refused upload keeps nothing except the reason. Spool files that nobody completes are removed after twice the grant lifetime.

Retention is recorded per artifact, and the retention jobs enforce it (see [Retention](#retention)).

With `s3`, the agent asks for a direct upload first. It falls back to the relayed `PUT` described above when the store cannot be used (see [Direct upload](#direct-upload)). The protocol steps are in [the protocol reference](protocol.md#artifact-uploads).

The spool is local to one server instance. With several instances, send a given upload's `PUT` and `complete` to the same instance. Sharing one spool directory between instances is not tested. A direct upload needs no spool entry before `complete`, so this does not affect it.

## Retention

Fleet keeps what you need to debug last week's failure, and to see this year's trend. It lets the rest go.

One server instance at a time runs the jobs below. That instance is the **leader**. It holds a Postgres advisory lock, and the other instances stand by. The leader runs the jobs safely beside ingest. Every job is idempotent. It logs what it did at `info` (`telemetry rolled up`, `artifacts expired`, `blobs deleted`, `expired data dropped`). It logs at `warn` when it fails, and retries at its next turn.

| Data | Kept | How it goes |
|---|---|---|
| Raw telemetry samples (5 s) | 7 days (`retention.raw_samples_days`) | The daily partition is detached and dropped once its whole UTC day is old enough |
| One-minute rollups | 90 days | Rows older than that are deleted |
| One-hour rollups | 1 year | Rows older than that are deleted |
| Raw agent journal, step executions | 14 days | Daily partitions are dropped. The flow outcomes, fingerprints and history stay |
| Audit log | 1 year (`retention.audit_days`) | Rows older than that are deleted |
| Artifacts | by retention class, below | Marked `expired`, so they leave the flow's list at once. The row stays as a tombstone for `retention.expired_artifact_days`, then is deleted |
| Artifact bytes (blobs) | until nothing refers to them, plus `retention.blob_grace_hours` | The object is deleted from the blob store |

**Rollups** summarize raw samples into one-minute and one-hour buckets. Each bucket keeps the count (`n`), the average and the maximum. A minute is rolled up two minutes after it ends, so batches in flight settle. A sample that arrives after its minute was rolled up re-rolls only the buckets it touches. That happens when an agent replays its spool after a reconnect.

A bucket's value is the average of the readings that exist. A metric an agent never reported stays *unavailable* in the rollups and in the API. It is never zero. The dashboard serves ranges of up to 24 hours from raw samples. It serves up to 30 days from one-minute rollups, and longer ranges from one-hour rollups. So history outlives the raw data.

**Raw partitions are never dropped before they are rolled up.** If the rollup job is behind, or was down, when a partition expires, the partition is kept. The server logs a `keeping an expired partition` warning. A later pass drops it.

**Artifact classes.** Each artifact gets a retention class when it arrives. The class decides when it expires. The defaults are seeded per project in the `retention_classes` table. Change a project's window for a class on the [Administration](#administration) page. A changed window applies to artifacts that become available afterwards. There is no server-wide window for these classes, so the page's value is the one a project uses. The defaults are:

| Class | Default | Applies to |
|---|---|---|
| `success_evidence` | 7 days | Screenshots, commands and UI hierarchies of a flow that has not failed |
| `failure_evidence` | 90 days | The same, once the flow fails (promoted when the flow finalizes, or at upload if it had already failed). Also the run's runner log (`maestro.log`) and device logcat excerpt, when any of its flows failed |
| `report` | 1 year | JUnit reports and HTML reports |
| `debug_log` | 30 days | Runner logs (`maestro.log`) of runs without a failed flow |
| `user_media` | 14 days | Maestro's own screenshots and recordings of a failed flow. Not promoted: the switch is opt-in, and they are never redacted |
| `video`, `bundle` | 14 days | Reserved |

A window of 0 days on the page keeps a class for ever. It is stored as a `NULL` TTL. A changed window applies to artifacts that become available afterwards, because an artifact's expiry is fixed when it becomes available. Promotion never shortens an artifact's life. An artifact with `legal_hold` set never expires. Upload requests that nobody completed expire too, a day after their grant lapses.

**Blobs are deleted only when nothing refers to them.** Bytes are stored once per project, however many artifacts name them. So an expired artifact's blob is deleted only after the *last* artifact using it has expired. It is then deleted only after the grace period.

Before deleting, the collector checks again that no artifact refers to the blob. That includes a pending upload of the same bytes. An upload that arrives while a blob is being deleted waits for the deletion to finish. A delete that fails, because the blob store is down, is retried by the next round.

**Leftover files.** On the local filesystem backend, a crash during an upload can leave a temp file in `<storage.path>/.tmp`. Files there older than `retention.stale_upload_hours` are removed. For S3, the bucket's lifecycle rule cleans unfinished multipart uploads (`AbortIncompleteMultipartUpload`). Fleet does not touch them.

Retention removes data from the live database and blob store. Backups made before a deletion still contain the data.

## Usercontent domain

Maestro HTML reports and raw artifacts come from CI jobs, so Fleet treats them as hostile. It never serves them from the dashboard's origin. It serves them only from a **second hostname**, `usercontent.url`. It serves them only through **signed links**. The dashboard's API hands out those links, after it checks that the caller may read artifacts (`GET /api/v1/projects/{project}/flows/{id}/artifacts`).

Use a different *registrable domain* from the dashboard. For example, use `fleet.example.com` with `fleet-usercontent.example.net`, not `usercontent.fleet.example.com`. Then no cookie, `document.domain` trick or same-site request can reach the dashboard from an uploaded file. Fleet refuses a configuration where both names are the same host. Get a certificate for the second name, and point both names at the proxy:

```toml
[api]
public_url = "https://fleet.example.com"

[usercontent]
url = "https://fleet-usercontent.example.net"
# signing_key: set FLEET_USERCONTENT__SIGNING_KEY_FILE (openssl rand -base64 48)
link_ttl_seconds = 300
```

What the origin guarantees, on every response:

- A link names one artifact of one project, until one moment. The format is `/u/<project>/<artifact>/<expires>/<signature>/<file name>`. The signature is an HMAC-SHA256 under your key. A changed, expired or foreign link gets 403, 410 or 404. Rotating the key invalidates every link.
- Every response carries `Content-Security-Policy: sandbox; default-src 'none'; …`. HTML reports get an opaque origin and no scripts.
- Every response carries `X-Content-Type-Options: nosniff`. The `Content-Type` is what the verified bytes are, not what the uploader said.
- Images, videos and sandboxed HTML are `Content-Disposition: inline`. Everything else is `attachment`.
- A single `bytes=` range gets `206 Partial Content` with a `Content-Range`. A range the file cannot give gets `416` with `Content-Range: bytes */<size>`. Several ranges, another unit, a malformed range or an `If-Range` header get the whole file (`200`), as HTTP allows. Every answer carries `Accept-Ranges: bytes`.
- No cookie is read or set. Only `/u/…` links are served. The API, the dashboard and the agent routes answer 404 on this host.
- The origin sends `Cross-Origin-Resource-Policy: cross-origin`. The dashboard embeds screenshots, and the signed link is the access control. `Access-Control-Allow-Origin` and `frame-ancestors` allow only `api.public_url`.

The dashboard's own policy names this origin, and only this one, in `img-src`, `media-src` and `connect-src`. `img-src` covers screenshots. `media-src` covers Maestro's own recordings, which play in the page's player. `connect-src` covers the log viewer, which fetches `maestro.log` and `commands.json` as text. No other directive, such as scripts, styles or frames, allows the origin. A browser that reports a CSP violation on the flow page usually means `usercontent.url` does not match the address people reach it at.

Without `usercontent.url`, artifacts are stored but cannot be opened. The flow page says so. Fleet routes by the `Host` header on its main listener. So a proxy that sends both hostnames to `fleet:8080` is enough. See [`deploy/compose/Caddyfile`](https://github.com/moyaver/fleet/blob/main/deploy/compose/Caddyfile). `usercontent.listen` serves the origin on a separate port instead.

## Shutdown

On SIGTERM, the server stops accepting connections. It finishes each agent's in-flight batch and sends `goaway`. It writes the last telemetry checkpoint, then closes the sockets with 1012. Each socket waits up to one second for the agent's close frame.

Agents reconnect, and resume where the server says. All state is in Postgres, so a restart loses nothing that was acknowledged. Give the container a stop grace period of 30 s.

## Upgrades

Upgrade the server first, then the agents. The exception is the move from the alpha releases, which is a clean break (below). Read the [CHANGELOG](https://github.com/moyaver/fleet/blob/main/CHANGELOG.md) from your version to the new one. It lists the steps that need action before you start.

### Clean break from the alpha releases

Before v1.0.0, the wire protocol and the schema have no compatibility promise (ADR-0014). The alpha releases cannot be upgraded. The move from `v0.1.0-alpha.1` through `v0.1.0-alpha.3` to `v0.1.0-alpha.4` has no upgrade path, because `v0.1.0-alpha.4` carries runner-neutral names (#153):

- The migrations were edited in place. An alpha database has the old names, and the new server refuses it. Start from a fresh database. The alpha data is not carried over.
- The alpha agents write the old names, which the server no longer reads. Upgrade the agents and the server together. Do not run an alpha agent against the new server.

### Before you upgrade

1. **Back up** the database and the blob store, with the configuration and secrets ([Backup and restore](#backup-and-restore)). The backup is the only way back once the schema has moved. See [Rollback](#rollback).
2. **Pin the image by tag.** Set `FLEET_IMAGE` in the Compose `.env` to the release, for example `ghcr.io/moyaver/fleet:0.1.0-alpha.4`. Do not follow `latest` across upgrades.

### Upgrade the server

In the Compose reference deployment, change `FLEET_IMAGE` to the new tag. Then run:

```bash
cd deploy/compose
docker compose pull && docker compose up -d
docker compose ps        # the fleet service is healthy when `fleet healthcheck` passes
docker compose exec fleet fleet --version   # the new version
```

On start, the server applies the pending migrations (`database.migrate = "auto"`, the default). It creates the next days' partitions, then serves. Migrations run under Postgres's advisory lock, so instances that start together are safe.

To run migrations yourself, set `FLEET_DATABASE__MIGRATE=manual`. The shipped `docker-compose.yml` pins `auto`. So set the variable in a `docker-compose.override.yml` beside it:

```yaml
services:
  fleet:
    environment:
      FLEET_DATABASE__MIGRATE: manual
```

With manual migrations, `serve` refuses an outdated schema. A plain `up -d` leaves the new container restarting. So pull the image, stop the old server, apply the migrations as their own step, and start the new one:

```bash
docker compose pull
docker compose stop fleet
docker compose run --rm fleet migrate
docker compose up -d
```

A server refuses a database whose schema is newer than its own. So an old image cannot run against a migrated database.

### Migrations

- Every migration is a plain `NNNN_name.sql` file in the repository's `migrations/` directory. Each one runs in its own transaction. A failed migration leaves the schema as it was.
- **Migrations are forward-only.** There are no down migrations. The schema never moves back. Rolling back is a restore ([Rollback](#rollback)).
- **From v1.0.0 on, applied migrations are never edited.** sqlx stores a checksum of each applied migration. The migrate step fails when an applied file no longer matches its checksum. A fix is a new migration.
- Before v1.0.0, migrations may be edited in place, because no database outside development holds them. The alpha releases cannot be upgraded (see the clean break above). ADR-0014 records the decision.
- From v1.0.0 on, each release's migrations are the previous release's plus new files. An upgrade applies the new ones in order. The migrations of each tag are in `migrations/` at that tag.

### Agents and server versions

- Agents and the server negotiate one wire protocol, `PROTOCOL_VERSION` (`1` in every release so far). An agent's `hello` names the range it speaks. The server accepts the connection only when its own version is in that range. Otherwise it answers `reject` with `unsupported_protocol`. That is fatal for the run: the agent stops reporting to Fleet, and Maestro is unaffected.
- Within one protocol version, changes are additive optional fields. Neither side rejects a field it does not know. A frame an agent cannot decode is dropped. So a newer agent and an older server, or the reverse, keep working on the same version.
- A change that is not additive bumps the protocol version. Then the server must be upgraded before the agents that need the new version. That is why the server goes first. Agents on the old version are refused until they are upgraded.
- The runner-neutral names ([ADR-0014](https://github.com/moyaver/fleet/blob/main/docs/decisions/0014-runner-neutral-names.md)) are not read by an older agent or server. So the agents and the server of one release move together. The protocol version stays `1`.

### Rollback

The schema does not move back. The previous release refuses the upgraded schema, because it does not know the migrations the upgrade added. It exits at start with "the database schema is newer than this build". So rolling back is a restore:

1. Stop the new server: `docker compose stop fleet`.
2. Set `FLEET_IMAGE` back to the previous tag. Restore the backup taken before the upgrade, as in [Restore](#restore). Use a new stack under its own Compose project name. The upgraded volumes then stay intact until you have checked the rollback.
3. Start the previous image. Check that the runs you know are there.

Everything recorded after the backup is lost from the database. Agents hold unacknowledged events in their spool. They resend what the restored server asks for, its `committed_through`, for as long as the spool still has those events. While the protocol version is unchanged, agents can stay on their version. They work with either server.

### Rehearse the upgrade

Operators can rehearse on a copy. Restore the latest backup into a stack under its own Compose project name (see [Restore](#restore)). Set `FLEET_IMAGE` there to the new tag. Then check three things: that the server starts, that `SELECT count(*) FROM artifacts` matches the source, and that a run you know lists in the dashboard.

Maintainers can rehearse the migrations without a deployment. Run `cargo xtask upgrade-rehearsal`. It does three things:

- It exports the `migrations/` of every release tag from v1.0.0 on, from git. Fetch the tags first with `git fetch --tags`. The alpha tags are skipped (see the clean break above). With no release from v1.0.0 on, it reports that there is nothing to rehearse, and it succeeds.
- It seeds rows under each release's schema, and then migrates to the current tree.
- It checks that every row is unchanged. It checks that the current read paths return the seeded runs, flows and artifacts. It checks that the release's own migrator refuses the upgraded schema.

The rehearsal takes a few seconds once Postgres 18 is up. It uses `DATABASE_URL`, or a throwaway container when that is unset. CI runs it after `cargo xtask ci`, in the main job, on that job's Postgres. The job's checkout fetches every tag (`fetch-depth: 0`). The test is `crates/fleet-store/tests/upgrade.rs`.

### Verified

Before the runner-neutral rename (ADR-0014), on 2026-11-08, the Compose reference deployment went from `v0.1.0-alpha.1` to `v0.1.0-alpha.3` with the steps above. That path no longer applies: alpha installs cannot be upgraded to alpha.4 (see the clean break above). The record: a backup, a project, an API key and a run were created, and after the upgrade migrations `0015` to `0017` were applied, the data was still there, the server reported `0.1.0-alpha.3` and new agents were accepted. The manual-migration path was checked on a second stack: the outdated schema is refused, and the migrate-then-start order works. The older server refuses the upgraded schema.

## Backup and restore

Fleet keeps its state in two places. **Postgres 18** holds everything except artifact bytes. That covers runs, flows, telemetry rollups, the audit log, the artifact and blob index, and API keys and sessions (stored as hashes). The **blob store** holds the artifact bytes: screenshots, recordings, JUnit reports, logs and HTML reports. It keys each blob by project and SHA-256. A backup is both of these, plus the configuration and secrets needed to start the server again.

Backups hold user names, audit events and every artifact. Encrypt them at rest, and restrict who can read them. [Retention](#retention) removes data from the live database and blob store only. A backup keeps the data until you delete the backup. So keep backups no longer than the retention you promise.

### What to back up

| What | Compose deployment | How |
|---|---|---|
| Database | `postgres`, database `fleet` | `pg_dump -Fc` (custom format) |
| Blob store | Garage bucket `fleet-artifacts` | copy the bucket through the S3 API |
| Blob store, `local_fs` | the `storage.path` directory | copy the directory |
| Configuration | `fleet.toml`, `Caddyfile`, `.env`, `secrets/` | copy, and keep the secret values in your secret store |

Do not back up `artifacts.spool_dir` (the `fleet_spool` volume). It holds uploads waiting for verification. An upload cut off by the backup is abandoned, and its artifact expires.

The secrets a restore needs:

- **`secrets/s3_access_key_id` and `secrets/s3_secret_access_key`**: Fleet's S3 credentials. They are the key the bucket is created with, and they also appear in `secrets/garage.env`. Also **`secrets/garage_rpc_secret`**. Without them, the copied bucket cannot be read.
- **`secrets/postgres_password` and `secrets/database_url`**: the restored Postgres takes its password from `postgres_password` when its volume is first created. `database_url` must match it.
- **`secrets/viewer_token`**: the bearer token for the REST API and CI scripts. Keep it, or every script that uses it breaks.
- **`secrets/usercontent_key`**: signs links to uploaded files. A new key works, but it invalidates links in flight. Those links last `usercontent.link_ttl_seconds`, 5 minutes by default.
- **The `[login]` client secret** (`FLEET_LOGIN__CLIENT_SECRET_FILE`), if you use browser sign-in.

API keys and browser sessions are rows in Postgres. So the database dump carries them. There is nothing else to back up for them.

### Order: database first, then blobs

1. **Dump the database.** `pg_dump` reads one consistent snapshot while the server runs.
2. **Copy the blob store.** Finish within `retention.blob_grace_hours` (24 by default) of starting step 1.

The order follows from how the retention jobs work ([`gc.rs`](https://github.com/moyaver/fleet/blob/main/crates/fleet-store/src/gc.rs), [`retention.rs`](https://github.com/moyaver/fleet/blob/main/crates/fleet-server/src/retention.rs)):

- An artifact becomes `available` only after its bytes are stored.
- The collector deletes a blob only when no live artifact refers to it, and `retention.blob_grace_hours` has passed since the last one expired. So a blob that an `available` artifact in the dump refers to is still in the bucket for that long after the dump. A copy that finishes in that window has every blob those artifacts need. Expired artifacts in the dump may have lost their bytes. They are tombstones, and nothing serves them.
- Blobs written after the dump are copied too. After a restore, nothing refers to them. Nothing deletes objects the database does not know about, so they stay and take space. They break nothing. Delete them by hand if the space matters.

The reverse order is unsafe. An artifact that becomes `available` between the bucket copy and the dump would refer to bytes the copy does not have.

### Take a backup

Run this in `deploy/compose`. The server keeps running. The Garage container is reached from a tool container that shares its network namespace, so nothing is published on the host.

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
  -v "$BACKUP/blobs:/backup/blobs" amazon/aws-cli:2.39.6 \
  --endpoint-url http://127.0.0.1:3900 s3 sync s3://fleet-artifacts /backup/blobs
```

Then copy `$BACKUP` and the configuration off the host. For `local_fs`, dump the database as above, and copy `storage.path`, skipping `.tmp`. Blobs are written by an atomic rename. So copying a live directory is safe, in the same order.

### Restore

Restore into a new stack under its own Compose project name (`-p`). That gives it its own volumes. Do not restore over a running server. `pg_restore --clean` replaces the database objects underneath it. Use the same `.env` and `secrets/` as the backup. Use the image that wrote the backup, or a newer one, because migrations only move forward.

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
  -v "$BACKUP/blobs:/backup/blobs" amazon/aws-cli:2.39.6 \
  --endpoint-url http://127.0.0.1:3900 s3 sync /backup/blobs s3://fleet-artifacts
docker compose -p fleet-restore up -d --wait fleet
```

The restored stack in this sequence has no proxy. Start one when the restored stack is the live one. Do that on its own host, or with the old stack stopped, because both publish ports 80 and 443. Then point the agents' `FLEET_SERVER` and the DNS name at it.

Runs that were in progress at the dump come back `active`. Once their agents stop reporting, they are detached. They are abandoned after `liveness.resume_grace_ms`, unless an agent resumes them. Then their open flows become `unknown`, and so do their open steps. Each such step shows Inferred. Nothing becomes success.

### Verify a restore

- Run [`deploy/compose/backup-restore-check.sh`](https://github.com/moyaver/fleet/blob/main/deploy/compose/backup-restore-check.sh). It runs the whole sequence above against throwaway stacks, `fleet-backup-src` and `fleet-backup-dst`, or `SRC_PROJECT` and `DST_PROJECT` if you set them. It checks that a row written before the backup is in the restored database. It checks that the REST API lists the project, and that the bucket inventory matches the source. It writes an API key and a canary object to the source stack, so do not point it at production. It removes both stacks and their volumes when it finishes.
- On a restored live stack, compare the state with what you recorded before the backup. Run `docker compose exec -T postgres psql -U fleet -d fleet -c 'SELECT count(*) FROM artifacts'`, and open a run you know. `curl -H "Authorization: Bearer $(cat secrets/viewer_token)" https://<FLEET_DOMAIN>/api/v1/projects/<slug>/attempts` lists it. Its flow page in the dashboard shows the screenshots, which come from the blob store.

## Load testing

`fleet-loadgen` drives simulated agents through the real protocol. Its flags include `--agents`, `--devices`, `--storm` (a reconnect storm) and `--duplicates` (resent and reordered batches). It exits with an error unless every event is acked.

Against a deployment behind TLS, use a `wss://` server URL. If the proxy's CA is not publicly trusted, pass it with `--ca-file`, or with `FLEET_CA_FILE`. See [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md#load-test-a-deployment).
