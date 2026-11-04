# Operator guide

## Deploy

The reference deployment is Docker Compose: Caddy (TLS, WebSocket-friendly), the Fleet server, Postgres 18 and Garage (S3-compatible, used for artifacts from M5). Step-by-step: [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md). The image is built from [`deploy/docker/Dockerfile`](https://github.com/moyaver/fleet/blob/main/deploy/docker/Dockerfile) (cargo-chef, distroless, runs as non-root).

## The `fleet` binary

| Command | Does |
|---|---|
| `fleet serve` | Serves the dashboard, the agent protocol and the API |
| `fleet migrate` | Applies database migrations and exits |
| `fleet check-config` | Validates the configuration and prints it, secrets redacted |
| `fleet healthcheck` | Exits 0 if the local server is ready (`/readyz`); for Docker `HEALTHCHECK` |
| `fleet api-key create --project <slug> --name <n>` | Prints a new API key once (`mf_live_…`); only its hash is stored |

## Configuration

Defaults, then a TOML file (`--config` or `FLEET_CONFIG`), then `FLEET_*` variables (`__` separates sections: `FLEET_DATABASE__URL`), then `FLEET_*_FILE` variables naming a file that holds the value (Docker and Kubernetes secrets). Errors name the variable or key at fault.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Address to listen on (the image sets `0.0.0.0:8080`) |
| `database.url` | — | `postgres://…` (use `FLEET_DATABASE__URL_FILE`) |
| `database.migrate` | `auto` | `auto` migrates on start; `manual` refuses an outdated or newer schema |
| `ingest.heartbeat_interval_ms` | `10000` | Agent heartbeat cadence (sent to agents in `welcome`) |
| `ingest.ping_interval_ms` | `20000` | Server pings, below common 60 s proxy idle cuts |
| `ingest.idle_timeout_ms` | `45000` | A silent agent socket is closed and its run detached |
| `ingest.telemetry_flush_ms` | `15000` | Telemetry checkpoint (10–30 s), then `telemetry_ack` |
| `ingest.shutdown_grace_ms` | `15000` | Drain time for sockets on SIGTERM |
| `liveness.stale_after_ms` | `25000` | A worker shows `stale` (display only) |
| `liveness.resume_grace_ms` | `900000` | A detached run is abandoned; its open flows become `unknown` |
| `api.viewer_tokens` | — | Bearer tokens that may read every project (scripts, tests); people sign in with `[login]` |
| `api.public_url` | — | The dashboard's origin; browser sockets from other origins are refused |
| `login.*` | — | Browser sign-in with your identity provider; see [Browser sign-in](#browser-sign-in) |
| `storage.backend` | `local_fs` | Where artifacts live: `local_fs` or `s3`; see [Artifact storage](#artifact-storage) |
| `storage.path` | `/var/lib/fleet/blobs` | Blob directory for `local_fs` |
| `storage.s3.*` | — | Endpoint, region, bucket and credentials for `s3` |
| `artifacts.spool_dir` | `/var/lib/fleet/spool` | Where uploaded bytes wait for verification (local disk; one server instance per spool) |
| `artifacts.grant_ttl_seconds` | `600` | How long an upload grant lasts |
| `artifacts.max_run_bytes`, `artifacts.max_run_files` | 2 GiB, 2000 | What one agent run may store |
| `artifacts.max_bytes.<class>` | 20 MiB screenshot, 16 MiB commands, 32 MiB junit, 64 MiB log, 128 MiB html report | Size limit per class |
| `usercontent.url` | — | The usercontent origin; see [Usercontent domain](#usercontent-domain) |
| `usercontent.signing_key` | — | HMAC key for signed links, at least 32 bytes (`FLEET_USERCONTENT__SIGNING_KEY_FILE`) |
| `usercontent.link_ttl_seconds` | `300` | How long a signed link works |
| `usercontent.listen` | — | Also serve the usercontent origin on its own address |
| `oidc.issuers` | GitHub, audience `fleet` | CI OIDC issuers accepted by `POST /api/v1/agent/token` |
| `projects[].trust_policies[]` | — | Who may publish with an OIDC token (repository and owner IDs, trusted refs, PR policy) |

## Browser sign-in

People sign in to the dashboard through your identity provider (OpenID Connect, authorization code with PKCE). Fleet is a confidential client: register it with the redirect URI `<api.public_url>/api/v1/auth/callback`. The provider must sign ID tokens with RS256 (Entra ID, Okta, Keycloak and Google do by default) and must send the user's groups in the ID token.

```toml
[api]
public_url = "https://fleet.example.com"

[login]
issuer = "https://login.example.com/realms/corp"   # discovery: <issuer>/.well-known/openid-configuration
client_id = "fleet"
# client_secret: set FLEET_LOGIN__CLIENT_SECRET_FILE; omit for a public (PKCE-only) client
group_claims = ["groups", "roles"]                  # "roles" carries Entra ID app roles
session_ttl_minutes = 480

[[login.group_roles]]                               # one project
group = "mobile-qa"
role = "viewer"                                     # viewer | maintainer | admin
project = "mobile-app"

[[login.group_roles]]                               # no project: every project (admin: a global admin)
group = "fleet-admins"
role = "admin"
```

- **Roles come from groups, at sign-in.** A user's effective role on a project is the highest role granted by their groups (or bound to them directly) for that project or for all projects. A user whose groups grant nothing is refused, and so is an ID token with none of the `group_claims`: Fleet never guesses. Entra ID omits `groups` for users in more than 200 groups; use app roles (`roles`) there. A removed group membership takes effect at the next sign-in, within `session_ttl_minutes`.
- **Sessions** are an opaque cookie (`__Host-fleet_session`: `HttpOnly`, `Secure`, `SameSite=Lax`) stored in Postgres by hash. Mutating requests made with the cookie must send the session's CSRF token (`X-CSRF-Token`, from `GET /api/v1/auth/session`) and, when the browser sends an `Origin`, the dashboard's own.
- A project the user has no role on looks exactly like a project that does not exist.

## Artifact storage

Screenshots, JUnit reports, logs, HTML reports and debug bundles are stored as **content-addressed blobs**: a blob's key is its project and the SHA-256 of its bytes. Uploading the same bytes twice to one project stores them once, and one project can never read or delete another project's blobs. A blob is only stored if its size and SHA-256 match what was declared; a mismatch stores nothing. The backend is chosen once, with `storage.backend`. Agents upload through the server (relayed: the store never needs to be reachable from runners or browsers); see [Artifact uploads](#artifact-uploads). The blob store is opened on first use, and the server logs a warning at start-up if it can't be opened.

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

Fleet speaks only the generic S3 API, path-style, with static credentials, so any S3-compatible store works. **Garage** is the reference (the Compose file runs it; its S3 port is `3900` and its credentials are `GARAGE_ACCESS_KEY` and `GARAGE_SECRET_KEY` from `.env`, bucket `fleet-artifacts`, region `garage`). **SeaweedFS** (Apache-2.0) is the alternative if AGPL is unacceptable for the store. Garage is a separate service reached over the network: its AGPL obligations apply to Garage only.

- The bucket needs no versioning. Give it a lifecycle rule that aborts incomplete multipart uploads after a day: an upload cut off mid-way (server restart, client gone) leaves its uploaded parts there until then. Blobs up to 8 MiB are written with one request and never leave parts behind.
- Fleet makes no request when it starts, so a wrong endpoint or key shows on the first upload, not in `check-config`.
- The same contract test suite runs against the filesystem backend and against Garage (v2.5.0, pinned by digest) in CI, so a change that breaks either fails the build.

### What is checked before a blob is stored

An artifact is accepted only if its declared size and SHA-256 match, it is not larger than 512 MiB, and its content (the magic bytes, never the file name or `Content-Type`) is allowed for its class:

| Class | Accepted content |
|---|---|
| screenshot | PNG, JPEG, WebP |
| junit | XML |
| html report | an HTML file, or a zip |
| log | UTF-8 text without NUL bytes |
| bundle | zip, or `tar.zst` |

Zip and `tar.zst` archives are also read through (never unpacked to disk) and refused if they have more than 5,000 entries, expand to more than 1 GiB in total, expand more than 100:1 (judged per file in a zip, over the whole stream for `tar.zst`, above 1 MiB), contain an absolute path, `..`, a backslash, a drive letter, control characters, a symlink, hardlink, device or fifo, contain another archive, or have bytes after their end. Sizes in archive headers are ignored: only bytes actually produced by decompression count. A refused artifact is refused with its reason and nothing is kept.

## Artifact uploads

Agents upload four kinds of evidence: failure screenshots, each flow's `commands.json`, the JUnit report and `maestro.log` (HTML reports are accepted by the server and served, but the agent does not upload them yet). The server accepts only its allowlisted classes, with the size limits above, and each run is capped at 2 GiB and 2,000 files. An upload is a request, a `PUT` of the bytes (spooled to `artifacts.spool_dir`), and a completion that verifies the bytes (see the table above) before the blob is written. Each upload grant belongs to the credential that asked, is short-lived and takes the bytes once; the same file requested again by a run is the same artifact, and bytes a project already holds for a class are not sent again. Nothing about a refused upload is kept except the reason. Spool files nobody completes are removed after twice the grant lifetime.

Retention is recorded per artifact (`success_evidence` 7 days, `failure_evidence` 90 days, `report` 1 year, `debug_log` 30 days; screenshots and commands of an already failed flow start as `failure_evidence`). Enforcement and blob garbage collection arrive with the retention job.

The spool is local: with several server instances, send a given upload's `PUT` and `complete` to the same instance (or share the spool directory).

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
- No cookie is read or set, and nothing but `/u/…` links is served: the API, the dashboard and the agent routes answer 404 on this host.
- `Cross-Origin-Resource-Policy: cross-origin` (the dashboard embeds screenshots; the link is the access control), and `Access-Control-Allow-Origin` and `frame-ancestors` allow only `api.public_url`.

Without `usercontent.url` artifacts are stored but cannot be opened. Fleet routes by the `Host` header on its main listener, so a proxy that sends both hostnames to `fleet:8080` is enough ([`deploy/compose/Caddyfile`](https://github.com/moyaver/fleet/blob/main/deploy/compose/Caddyfile)); `usercontent.listen` serves the origin on a separate port instead.

## Shutdown and upgrades

On SIGTERM the server stops accepting connections, finishes each agent's in-flight batch, sends `goaway`, writes the last telemetry checkpoint and closes sockets with 1012; agents reconnect and resume where the server says. All state is in Postgres, so a restart loses nothing that was acknowledged. Give the container a stop grace period of 30 s.

Back up first, then upgrade the server (migrations run, or run `fleet migrate`), then the agents.

## Load testing

`fleet-loadgen` drives simulated agents through the real protocol (`--agents`, `--devices`, `--storm` for a reconnect storm, `--duplicates` for resent and reordered batches) and fails unless every event is acked. Against a deployment behind TLS use `wss://` and point `SSL_CERT_FILE` at the proxy's CA if it is not publicly trusted; see [`deploy/compose/README.md`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md#load-test-a-deployment).
