# Try Fleet on a free VM

This page runs the reference Compose deployment ([`deploy/compose`](https://github.com/moyaver/fleet/blob/main/deploy/compose/README.md)) on one Arm virtual machine from a cloud free tier, with two free DuckDNS names. It is for trying Fleet and for test data. It is not a production setup: there is one host, no redundancy, and no backups unless you set them up ([backup and restore](operator-guide.md#backup-and-restore)).

Replace `<dashboard>` and `<uploads>` below with your own names.

## 1. Create the VM

- Use Oracle Cloud's Always Free tier, which includes Arm (Ampere A1) compute.
- Check Oracle's current limits before you create the instance. The limits change. At the time of writing, the allowance was 2 OCPUs and 12 GB of memory in total.
- Create one instance with shape `VM.Standard.A1.Flex`, 2 OCPUs and 12 GB (or less), and an Ubuntu LTS image for arm64. Give it a public IPv4 address.
- Check Oracle's Always Free terms. They allow idle instances to be reclaimed.

## 2. Open ports 80 and 443

Ports 80 (the certificate challenge and redirect) and 443 (HTTPS) must reach the VM from the internet. Open them in two places:

1. Add ingress rules for TCP 80 and TCP 443 from `0.0.0.0/0` to the VCN security list, or to a network security group.
2. Open the same ports in the Ubuntu image's own firewall (iptables). List the chain first with `sudo iptables -L INPUT -n --line-numbers`. Insert the rules above any `REJECT` rule. The commands below insert at line 6. Choose a lower line number if a `REJECT` rule sits above line 6:

```bash
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 80 -j ACCEPT
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 443 -j ACCEPT
sudo apt-get install -y iptables-persistent   # only if netfilter-persistent is missing
sudo netfilter-persistent save
```

## 3. Get two names

Create two DuckDNS names, each pointing at the VM's public IP:

- Use `<dashboard>.duckdns.org` for the dashboard and the API.
- Use `<uploads>.duckdns.org` for uploaded files.

duckdns.org is on the Public Suffix List, so the two names are separate sites. Uploaded files must not share a site with the dashboard ([usercontent domain](operator-guide.md#usercontent-domain)). Caddy requests certificates for both names on first start, so both must resolve to the VM before then.

## 4. Install Docker

Install Docker Engine and the Compose plugin from Docker's apt repository ([instructions](https://docs.docker.com/engine/install/ubuntu/)). Then add your user to the `docker` group, and log in again:

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

Pull the server image, the default `FLEET_IMAGE` (`ghcr.io/moyaver/fleet:0.1.0-alpha.4`), and start the stack. It is the release that carries the clean break in the [operator guide](operator-guide.md#clean-break-from-the-alpha-releases), so its agents and server match:

```bash
docker compose pull
docker compose up -d
docker compose ps        # wait until fleet is healthy
```

To build the server from this clone instead, give the local build its own image name, so it is not mistaken for the published one: `echo 'FLEET_IMAGE=fleet:source' >> .env`, then `docker compose up -d --build`. The build compiles the server and the dashboard inside Docker, so it takes a long time on two Arm cores.

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

- Set up **browser sign-in** with your identity provider in the `[login]` section of `fleet.toml` ([browser sign-in](operator-guide.md#browser-sign-in)).
- Turn on **anonymous reads** for test data only. The `fleet.toml` in the repository has no `[api]` table. Add these lines to the end of `deploy/compose/fleet.toml`, then run `docker compose restart fleet`:

  ```toml
  [api]
  anonymous_read = true
  ```

  Anyone who can reach the dashboard can then read every run. Anonymous reads are not rate limited. Do not use them with real application data.

Scripts can use the REST API with the viewer token in `secrets/viewer_token`:

```bash
curl -H "Authorization: Bearer $(cat secrets/viewer_token)" https://<dashboard>.duckdns.org/api/v1/projects/<project-slug>/attempts
```

## 7. Connect CI

Create an API key for the project and store it as the CI secret `FLEET_TOKEN`. The server prints the key once, so copy it then:

```bash
docker compose run --rm fleet api-key create --project <project-slug> --name ci
```

In the CI job, set `FLEET_SERVER=https://<dashboard>.duckdns.org`. Then wrap the Maestro command with `fleet-agent run --`. See the [user guide](user-guide.md).

The agent is the matching `v0.1.0-alpha.4` release. The release installer and the composite action both install it. The [runner guide](runner-guide.md#install) has the installer command. To build the agent from this repository instead:

```bash
cargo build --locked -p fleet-agent     # target/debug/fleet-agent, on the runner or your machine
```

In GitHub Actions, check out this repository next to your app, build the agent, and run the composite action from that checkout with `install: "false"`, which runs the `fleet-agent` already on `PATH`. The repository's own CI runs this pattern in the `action-smoke` job of `.github/workflows/ci.yml`:

```yaml
steps:
  - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
  - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
    with:
      repository: moyaver/fleet
      path: fleet
  - run: cargo build --locked -p fleet-agent
    working-directory: fleet
  - run: echo "$GITHUB_WORKSPACE/fleet/target/debug" >> "$GITHUB_PATH"
  - uses: ./fleet/examples/github-actions/fleet-agent
    with:
      install: "false"
      command: maestro test .maestro/
```

Your job also needs `permissions: id-token: write` for the OIDC exchange, or `FLEET_TOKEN` set to the key above (see the [runner guide](runner-guide.md#credentials)).

## Limits of this setup

- Plan for one VM: one Postgres and one Garage, both on its disk.
- Expect data loss after a failed disk or a reclaimed instance, unless you have a backup.
- Take a backup ([backup and restore](operator-guide.md#backup-and-restore)) before you upgrade. Upgrade by changing `FLEET_IMAGE` in `.env`, then run `docker compose pull && docker compose up -d`.
