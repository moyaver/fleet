#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Runs one fleet-loadgen scenario against the Compose stack and records its raw numbers.
#
#   run.sh <name> [fleet-loadgen flags...]
#
# Writes research/14-assets/raw/<name>/: report.json (the loadgen's numbers), loadgen.out and
# loadgen.err, stats.csv (docker stats, once a second, while the run lasts), pg.txt (the database
# size before and after), server.log (the server's log since the run began) and host.txt (the
# host's load before the run). The host is shared and never idle, so a run starts whatever the load and records it (see research/14-load-test.md).
#
# Needs the stack up under the project name fleet-load141 (research/14-load-test.md), the loadgen
# built in release, and in the environment: FLEET_TOKEN (an API key of project mobile-app),
# FLEET_VIEWER_TOKEN (the viewer token) and FLEET_CA_FILE (the proxy's root CA, outside the repo).
# The credentials never reach the output directory.
set -eu

name=$1
shift
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out="$here/raw/$name"
proj=fleet-load141
loadgen="$repo/target/release/fleet-loadgen"
mkdir -p "$out"

load=$(sysctl -n vm.loadavg | awk '{print $2}')
echo "host_load_1m_before=$load" > "$out/host.txt"
echo "cores=$(sysctl -n hw.ncpu)" >> "$out/host.txt"

pg_size() {
  docker exec "${proj}-postgres-1" psql -U fleet -d fleet -Atc "select pg_database_size('fleet')"
}

before=$(pg_size)
start=$(date -u +%Y-%m-%dT%H:%M:%SZ)
containers="${proj}-fleet-1 ${proj}-postgres-1 ${proj}-proxy-1"

# One docker stats reading a second: CPU is per core (100% is one core), memory is the container's.
(
  while :; do
    now=$(date -u +%s)
    docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}}' $containers \
      | sed "s/^/$now,/"
    sleep 1
  done
) > "$out/stats.csv" 2>/dev/null &
sampler=$!

set +e
"$loadgen" --server wss://localhost --report "$out/report.json" "$@" \
  > "$out/loadgen.out" 2> "$out/loadgen.err"
rc=$?
set -e

kill "$sampler" 2>/dev/null || true
wait "$sampler" 2>/dev/null || true
after=$(pg_size)
docker compose -p "$proj" logs --no-color --since "$start" fleet > "$out/server.log" 2>&1 || true
printf 'pg_database_size_before=%s\npg_database_size_after=%s\n' "$before" "$after" > "$out/pg.txt"
echo "loadgen_exit=$rc" >> "$out/host.txt"
echo "host_load_1m_after=$(sysctl -n vm.loadavg | awk '{print $2}')" >> "$out/host.txt"
cat "$out/loadgen.out"
exit "$rc"
