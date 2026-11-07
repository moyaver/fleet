# Fleet with Docker Compose

The reference deployment: Caddy (TLS, WebSocket-friendly) → Fleet → Postgres 18, plus Garage as the S3-compatible artifact store (used from M5).

## Start

```bash
cd deploy/compose
cp .env.example .env            # then fill in the Garage values
mkdir -p secrets
openssl rand -hex 24 > secrets/postgres_password
printf 'postgres://fleet:%s@postgres:5432/fleet' "$(cat secrets/postgres_password)" > secrets/database_url
openssl rand -hex 32 > secrets/viewer_token
openssl rand -base64 48 > secrets/usercontent_key   # signs links to uploaded files
docker compose up -d --build
docker compose ps               # fleet turns healthy once migrations ran and Postgres answers
```

Each release tag publishes the server image to `ghcr.io/moyaver/fleet` (`latest` for stable releases only). The compose file uses `ghcr.io/moyaver/fleet:0.1.0-alpha.2` by default. To run another release instead of building, set `FLEET_IMAGE=ghcr.io/moyaver/fleet:<version>` in `.env`, then `docker compose pull && docker compose up -d` without `--build`.

Generate the Garage values in `.env` with `openssl rand -hex 32` for `GARAGE_RPC_SECRET` and `GARAGE_SECRET_KEY`, and with `GK` followed by `openssl rand -hex 12` for `GARAGE_ACCESS_KEY`.

Fleet migrates on start (`FLEET_DATABASE__MIGRATE=auto`). For change-controlled upgrades set it to `manual` and run `docker compose run --rm fleet migrate`; `serve` then refuses a schema that is behind, or newer than the binary.

## Credentials for CI

- **GitHub Actions:** add a `[[projects.trust_policies]]` entry to `fleet.toml` and give the workflow `permissions: id-token: write`. No secret is stored anywhere.
- **Other CI:** create an API key and store it as the CI secret `FLEET_TOKEN`:

  ```bash
  docker compose run --rm fleet api-key create --project mobile-app --name ci
  ```

Uploaded files (Maestro HTML reports, screenshots, logs) are served from a second hostname, `FLEET_USERCONTENT_DOMAIN`, which must resolve to the same proxy and have its own certificate; in production use a different registrable domain from `FLEET_DOMAIN`. See the [operator guide](../../docs/src/operator-guide.md#usercontent-domain).

Point agents at `FLEET_SERVER=https://<FLEET_DOMAIN>`. The REST API and live socket take the viewer token in `secrets/viewer_token` as a bearer token. Browser sign-in uses the `[login]` section of `fleet.toml` ([operator guide](../../docs/src/operator-guide.md#browser-sign-in)).

With `FLEET_DOMAIN=localhost` Caddy signs with its own CA. Export it for agents (`FLEET_CA_FILE`) and other clients:

```bash
docker compose cp proxy:/data/caddy/pki/authorities/local/root.crt ./caddy-root.crt
curl --cacert caddy-root.crt https://localhost/readyz
```

## Load-test a deployment

`fleet-loadgen` (in this repository) runs simulated agents through the proxy, so it exercises TLS, the WebSocket upgrade and the server together:

```bash
cargo build --release -p fleet-loadgen
SSL_CERT_FILE=caddy-root.crt FLEET_TOKEN=<API key> target/release/fleet-loadgen \
  --server wss://localhost --storm --duplicates
```

It exits non-zero unless every event was acked. Run `docker compose restart fleet` during a slower run (`--batch 4 --pace-ms 500`) to check that agents ride out a restart.

## Operate

- `docker compose run --rm fleet check-config` prints the effective configuration, secrets redacted.
- `docker stop` sends SIGTERM: agents get `goaway`, their last batch is acked, sockets close with 1012 and agents reconnect when Fleet is back. `stop_grace_period` is 30 s for that drain.
- Back up Postgres with `pg_dump -Fc`; the image mounts its data at `/var/lib/postgresql` (Postgres 18 layout).
- Backup and restore (Postgres and the Garage bucket, restored into a fresh stack): [operator guide](../../docs/src/operator-guide.md#backup-and-restore). `backup-restore-check.sh` runs the whole sequence against throwaway stacks.
