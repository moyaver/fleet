#!/usr/bin/env bash
# Installs the fleet-agent release named by AGENT_VERSION (see action.yml). Run by the composite
# action's install step, and by test-install.sh with curl, gh and tar stubbed.
set -euo pipefail
if ! [[ "$AGENT_VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  echo "::error::version must be a release tag such as v0.1.0-alpha.3, not '$AGENT_VERSION'"
  exit 1
fi
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  *)
    echo "::error::no fleet-agent release for $(uname -s) $(uname -m)"
    exit 1
    ;;
esac
base="https://github.com/moyaver/fleet/releases/download/$AGENT_VERSION"
archive="fleet-agent-$target.tar.xz"
dir="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/fleet-agent"
mkdir -p "$dir"
curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$dir/$archive" "$base/$archive"
# The expected digest comes from the build attestation, signed by the release workflow and
# recorded in Sigstore's transparency log. It does not come from sha256.sum, which is
# served from the same release as the archive and so proves nothing about it (research 06 T18).
case "$VERIFY_ATTESTATION" in
  true)
    if ! command -v gh > /dev/null 2>&1; then
      echo "::error::verify-attestation is true but the gh CLI is not installed; install gh or set verify-attestation: false"
      exit 1
    fi
    if ! command -v jq > /dev/null 2>&1; then
      echo "::error::verify-attestation is true but jq is not installed"
      exit 1
    fi
    if [ -z "$GH_TOKEN" ]; then
      echo "::error::verify-attestation is true but there is no token: grant attestations: read, or set github-token"
      exit 1
    fi
    attestation="$dir/attestation.json"
    if ! gh attestation verify "$dir/$archive" --repo moyaver/fleet --format json > "$attestation"; then
      echo "::error::the build attestation for $archive does not verify"
      exit 1
    fi
    expected=$(jq -r '.[].verificationResult.statement.subject[]?.digest.sha256 // empty' "$attestation")
    if command -v sha256sum > /dev/null 2>&1; then
      actual=$(sha256sum "$dir/$archive" | cut -d' ' -f1)
    else
      actual=$(shasum -a 256 "$dir/$archive" | cut -d' ' -f1)
    fi
    if [ -z "$expected" ] || ! grep -Fxq "$actual" <<< "$expected"; then
      echo "::error::checksum mismatch for $archive: its digest is not among those in the build attestation"
      exit 1
    fi
    ;;
  false)
    echo "::warning::build attestation not checked (verify-attestation: false): the archive is not authenticated"
    ;;
  *)
    echo "::error::verify-attestation must be true or false, not '$VERIFY_ATTESTATION'"
    exit 1
    ;;
esac
tar -xJf "$dir/$archive" -C "$dir"
bin="$dir/fleet-agent-$target"
"$bin/fleet-agent" version
echo "$bin" >> "$GITHUB_PATH"
