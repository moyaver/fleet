#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Backup and restore check for the Compose reference deployment (operator guide, "Backup and restore").
#
# 1. Starts a throwaway source stack (postgres, garage, fleet; no proxy, so no host ports).
# 2. Writes a known row (an API key) and a known object (a canary in the bucket).
# 3. Backs up: pg_dump first, then the bucket, in that order.
# 4. Restores both into a fresh stack with another Compose project name.
# 5. Checks the row, the REST API and the object inventory in the restored stack.
# 6. Tears both stacks down with their volumes.
#
# Needs .env and secrets/ as in README.md, and a Fleet image: FLEET_IMAGE=<image> (or a
# local build, which `up` makes when the image is missing). Projects default to
# fleet-backup-src and fleet-backup-dst; the script refuses a project that already has containers.
# It writes to the source stack, so do not point it at production.
set -eu

cd "$(dirname "$0")"
SRC=${SRC_PROJECT:-fleet-backup-src}
DST=${DST_PROJECT:-fleet-backup-dst}
AWS_IMAGE=amazon/aws-cli:latest

die() { echo "backup-restore-check: $*" >&2; exit 1; }
[ -f .env ] && [ -f secrets/viewer_token ] && [ -f secrets/database_url ] && [ -f secrets/s3_access_key_id ] || die "missing .env or secrets/ (see README.md)"
export AWS_ACCESS_KEY_ID="$(cat secrets/s3_access_key_id)" AWS_SECRET_ACCESS_KEY="$(cat secrets/s3_secret_access_key)" AWS_DEFAULT_REGION=garage
export AWS_REQUEST_CHECKSUM_CALCULATION=when_required AWS_RESPONSE_CHECKSUM_VALIDATION=when_required
TOKEN=$(cat secrets/viewer_token)
export TOKEN

dc() { p=$1; shift; docker compose --progress quiet -p "$p" "$@"; }
# A tool container that shares a service's network namespace, so it reaches the service on 127.0.0.1.
in_ns() { p=$1; svc=$2; shift 2; docker run --rm --network "container:$(dc "$p" ps -q "$svc")" "$@"; }
# The bucket through the S3 API, from the Garage container's network namespace.
s3() { p=$1; shift; in_ns "$p" garage -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION \
  -e AWS_REQUEST_CHECKSUM_CALCULATION -e AWS_RESPONSE_CHECKSUM_VALIDATION -v "$BACKUP:/backup" \
  "$AWS_IMAGE" --endpoint-url http://127.0.0.1:3900 "$@"; }

for p in "$SRC" "$DST"; do
  [ -z "$(dc "$p" ps -q)" ] || die "project $p already has containers; choose another name"
done
BACKUP=$(mktemp -d "${TMPDIR:-/tmp}/fleet-backup.XXXXXX")
mkdir "$BACKUP/blobs"
cleanup() {
  dc "$DST" down -v >/dev/null 2>&1 || true
  dc "$SRC" down -v >/dev/null 2>&1 || true
  rm -rf "$BACKUP"
}
trap cleanup EXIT

echo "== source stack $SRC"
dc "$SRC" up -d --wait postgres garage fleet
# A known row, written by the product's own CLI, and a known object in the bucket.
dc "$SRC" run --rm -T --no-deps fleet api-key create --project mobile-app --name backup-check >/dev/null
echo "canary written $(date -u +%FT%TZ)" > "$BACKUP/canary.txt"
s3 "$SRC" s3 cp /backup/canary.txt s3://fleet-artifacts/backup-check/canary.txt >/dev/null

echo "== backup: database first, then blobs"
dc "$SRC" exec -T postgres pg_dump -Fc -U fleet -d fleet > "$BACKUP/fleet.dump"
s3 "$SRC" s3 sync s3://fleet-artifacts /backup/blobs >/dev/null
s3 "$SRC" s3api list-objects-v2 --bucket fleet-artifacts --query 'Contents[].[Key,Size,ETag]' --output text \
  | sort > "$BACKUP/src.inventory"

echo "== restore into fresh stack $DST"
dc "$DST" up -d --wait postgres garage
dc "$DST" exec -T postgres pg_restore -U fleet -d fleet --no-owner --clean --if-exists < "$BACKUP/fleet.dump"
s3 "$DST" s3 sync /backup/blobs s3://fleet-artifacts >/dev/null
dc "$DST" up -d --wait fleet

echo "== checks"
rows=$(dc "$DST" exec -T postgres psql -U fleet -d fleet -tAc "SELECT count(*) FROM api_keys WHERE name = 'backup-check'")
[ "$rows" = 1 ] || die "the known api key is missing from the restored database"
echo "api key row: ok"
# shellcheck disable=SC2016 # $TOKEN is expanded inside the container
in_ns "$DST" fleet -e TOKEN alpine sh -c \
  'wget -qO- --header "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/projects' \
  | grep -q '"slug":"mobile-app"' || die "REST did not list the project"
echo "REST /api/v1/projects: ok"
s3 "$DST" s3api list-objects-v2 --bucket fleet-artifacts --query 'Contents[].[Key,Size,ETag]' --output text \
  | sort > "$BACKUP/dst.inventory"
diff "$BACKUP/src.inventory" "$BACKUP/dst.inventory" || die "the restored bucket differs from the source"
grep -q backup-check/canary.txt "$BACKUP/dst.inventory" || die "the canary object is missing"
echo "bucket inventory: ok ($(wc -l < "$BACKUP/dst.inventory" | tr -d ' ') objects)"
echo "backup-restore-check: ok"
