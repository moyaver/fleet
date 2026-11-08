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
# Garage secrets (see "Garage secrets" below)
openssl rand -hex 32 > secrets/garage_rpc_secret
printf 'GK%s' "$(openssl rand -hex 12)" > secrets/s3_access_key_id
openssl rand -hex 32 > secrets/s3_secret_access_key
printf 'GARAGE_DEFAULT_ACCESS_KEY=%s\nGARAGE_DEFAULT_SECRET_KEY=%s\n' "$(cat secrets/s3_access_key_id)" "$(cat secrets/s3_secret_access_key)" > secrets/garage.env
chmod 600 secrets/garage.env
chmod 400 secrets/garage_rpc_secret
docker compose up -d --build
docker compose ps               # fleet turns healthy once migrations ran and Postgres answers
```

Each release tag publishes the server image to `ghcr.io/moyaver/fleet` (`latest` for stable releases only). The compose file uses `ghcr.io/moyaver/fleet:0.1.0-alpha.3` by default. To run another release instead of building, set `FLEET_IMAGE=ghcr.io/moyaver/fleet:<version>` in `.env`, then `docker compose pull && docker compose up -d` without `--build`.

## Garage secrets

Garage's secrets are files under `secrets/`, never values in `.env` (research 06 T20):

- `garage_rpc_secret`: Garage's RPC secret, read through `GARAGE_RPC_SECRET_FILE`. Keep the file `0400` on the host: Garage refuses a world-readable secret.
- `s3_access_key_id` and `s3_secret_access_key`: Fleet's S3 key pair, read through `FLEET_STORAGE__S3__*_FILE`. The key ID is `GK` followed by 24 hex characters.
- `garage.env`: the same pair as `GARAGE_DEFAULT_ACCESS_KEY` and `GARAGE_DEFAULT_SECRET_KEY`, which Garage needs to create the bucket's key at start. Garage 2.5 reads these only from the environment and has no file option for them, so this one file is the only copy of the pair in the container's environment. Keep it `0600`.

The two copies of the key pair must match. Rotating the S3 key means changing all three files and restarting Garage and Fleet.

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
