# Try Fleet on a free VM

This page runs the reference Compose deployment ([`deploy/compose`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md)) on one Arm virtual machine from a cloud free tier, with two free DuckDNS names. It is for trying Fleet and for test data. It is not a production setup: there is one host, no redundancy, and no backups unless you set them up ([backup and restore](operator-guide.md#backup-and-restore)).

Replace `<dashboard>` and `<uploads>` below with your own names.

## 1. Create the VM

- Oracle Cloud's Always Free tier includes Arm (Ampere A1) compute. At the time of writing its allowance was 2 OCPUs and 12 GB of memory in total. Check Oracle's current limits before you create the instance, because they change.
- Create one instance with shape `VM.Standard.A1.Flex`, 2 OCPUs and 12 GB (or less), and an Ubuntu LTS image for arm64. Give it a public IPv4 address.
- Oracle's Always Free terms also allow idle instances to be reclaimed. Check the current terms.

## 2. Open ports 80 and 443

Ports 80 (the certificate challenge and redirect) and 443 (HTTPS) must reach the VM from the internet, in two places:

1. The VCN security list (or a network security group): an ingress rule for TCP 80 and TCP 443 from `0.0.0.0/0`.
2. The Ubuntu image's own firewall (iptables). Look at the chain first with `sudo iptables -L INPUT -n --line-numbers`, then add the rules above any `REJECT` rule:

```bash
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 80 -j ACCEPT
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 443 -j ACCEPT
sudo netfilter-persistent save   # from the iptables-persistent package, if it is missing: sudo apt-get install -y iptables-persistent
```

## 3. Get two names

Create two DuckDNS names, each pointing at the VM's public IP:

- `<dashboard>.duckdns.org` for the dashboard and the API;
- `<uploads>.duckdns.org` for uploaded files.

duckdns.org is on the Public Suffix List, so the two names are separate sites. Uploaded files must not share a site with the dashboard ([usercontent domain](operator-guide.md#usercontent-domain)). Caddy requests certificates for both names on first start, so both must resolve to the VM before then.

## 4. Install Docker

Install Docker Engine and the Compose plugin from Docker's apt repository ([instructions](https://docs.docker.com/engine/install/ubuntu/)). Then let your user run it, and log in again:

```bash
sudo usermod -aG docker "$USER"
docker compose version
```

## 5. Configure and start Fleet

```bash
git clone https://github.com/moyaver/fleet.git
cd fleet/deploy/compose
cp .env.example .env
mkdir -p secrets
```

Run the commands in the [Start section of the Compose guide](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md#start) that create the secret files. They create eight files in `secrets/`, including `garage.env`, which Compose reads before it starts. The Garage keys are secret files, not `.env` values, so `.env` needs only the two names. Set the two names in `.env`:

```
FLEET_DOMAIN=<dashboard>.duckdns.org
FLEET_USERCONTENT_DOMAIN=<uploads>.duckdns.org
```

Keep `.env` and `secrets/`: a restore needs them.

Until the next release, build the server from this clone. The published image (the default `FLEET_IMAGE`, `ghcr.io/moyaver/fleet:0.1.0-alpha.3`) predates the clean break in the [operator guide](operator-guide.md#clean-break-from-the-alpha-releases), so it cannot serve this branch's agents. Give the local build its own image name, so it is not mistaken for the published one, and build:

```bash
echo 'FLEET_IMAGE=fleet:source' >> .env
docker compose up -d --build
docker compose ps        # wait until fleet is healthy
```

The build compiles the server and the dashboard inside Docker, so it takes a long time on two Arm cores. Once a release carries the clean break, the published image replaces it: set `FLEET_IMAGE` to that tag and leave out `--build`.

Check that the server answers through the proxy:

```bash
curl -f https://<dashboard>.duckdns.org/readyz
```

With the two DuckDNS names, Caddy gets its certificates from ACME and nothing else is needed. If you test without public DNS (for example `FLEET_DOMAIN=localhost`), Caddy signs with its own CA instead. Export that CA and check with it, as the [Compose guide](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md) shows for `FLEET_DOMAIN=localhost`:

```bash
docker compose cp proxy:/data/caddy/pki/authorities/local/root.crt ./caddy-root.crt
curl --cacert caddy-root.crt https://localhost/readyz
```

Give the agent the same file with `FLEET_CA_FILE=caddy-root.crt` (step 7).

## 6. Sign in, or use test data

The dashboard needs one of these:

- **Browser sign-in** with your identity provider, set in the `[login]` section of `fleet.toml` ([browser sign-in](operator-guide.md#browser-sign-in)).
- **Anonymous reads** for test data only. The `fleet.toml` in the repository has no `[api]` table: add these lines at the end of `deploy/compose/fleet.toml`, then restart with `docker compose restart fleet`:

  ```toml
  [api]
  anonymous_read = true
  ```
 Anyone who can reach the dashboard can then read every run, and anonymous reads are not rate limited. Do not use it with real application data.

Scripts can use the REST API with the viewer token in `secrets/viewer_token`:

```bash
curl -H "Authorization: Bearer $(cat secrets/viewer_token)" https://<dashboard>.duckdns.org/api/v1/projects/<project-slug>/attempts
```

## 7. Connect CI

Create an API key for the project and store it as the CI secret `FLEET_TOKEN`. The key is shown once:

```bash
docker compose run --rm fleet api-key create --project <project-slug> --name ci
```

Set `FLEET_SERVER=https://<dashboard>.duckdns.org` in the job, then wrap the Maestro command with `fleet-agent run --` ([user guide](user-guide.md)).

## Limits of this setup

- One VM, one Postgres and one Garage on its disk. A failed disk or a reclaimed instance loses the data unless you have a backup.
- Take a backup ([backup and restore](operator-guide.md#backup-and-restore)) before you upgrade. Upgrade by changing `FLEET_IMAGE` in `.env`, then `docker compose pull && docker compose up -d`.
