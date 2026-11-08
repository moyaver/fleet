#!/usr/bin/env bash
# Tests install.sh, the composite action's install step (research 06 T18): its checks of the release
# attestation. curl, gh and tar are stubs on PATH, and PATH holds no other gh, so nothing is
# downloaded and the host's gh cannot leak into a case. `cargo xtask ci` runs this script.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
install="$here/install.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The tools install.sh uses, linked into a directory of their own. gh is not among them.
tools="$work/tools"
mkdir -p "$tools"
for tool in bash sh env uname mkdir cut grep sed tr cat jq sha256sum shasum head rm dirname basename chmod wc tar; do
  if path=$(command -v "$tool" 2> /dev/null); then
    ln -s "$path" "$tools/$tool"
  fi
done
if ! command -v jq > /dev/null 2>&1; then
  echo "test-install: jq is required" >&2
  exit 1
fi

# The SHA-256 of a file given on stdin, with whichever tool the host has.
sha256_stdin() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum | cut -d' ' -f1
  else
    shasum -a 256 | cut -d' ' -f1
  fi
}

# The archive the stubbed download writes, and its digest.
archive_bytes='fake archive'
digest=$(printf '%s' "$archive_bytes" | sha256_stdin)

failures=0
cases=0

# A stub directory for one case. $1 is the case's directory; $2 is "gh" (a gh stub that answers
# as $3 says: ok, fail or wrong) or "nogh".
make_stubs() {
  stubs="$1/stubs"
  mkdir -p "$stubs"
  # curl: writes the archive it is asked for, and logs every URL it is given.
  cat > "$stubs/curl" << EOF
#!/bin/sh
out=""
while [ \$# -gt 0 ]; do
  case "\$1" in
    -o) out="\$2"; shift ;;
    http*) echo "\$1" >> "$1/urls" ;;
  esac
  shift
done
printf '%s' '$archive_bytes' > "\$out"
EOF
  chmod +x "$stubs/curl"
  # tar: creates the directory the release archive would unpack to, with a runnable agent in it.
  cat > "$stubs/tar" << 'EOF'
#!/bin/sh
archive=""
dest=""
while [ $# -gt 0 ]; do
  case "$1" in
    -C) dest="$2"; shift ;;
    *.tar.xz) archive="$1" ;;
  esac
  shift
done
name=$(basename "$archive" .tar.xz)
mkdir -p "$dest/$name"
printf '#!/bin/sh\necho "fleet-agent test version"\n' > "$dest/$name/fleet-agent"
chmod +x "$dest/$name/fleet-agent"
EOF
  chmod +x "$stubs/tar"
  if [ "$2" = gh ]; then
    case "$3" in
      ok)
        json="[{\"verificationResult\":{\"statement\":{\"subject\":[{\"name\":\"archive\",\"digest\":{\"sha256\":\"$digest\"}}]}}}]"
        ;;
      wrong)
        json='[{"verificationResult":{"statement":{"subject":[{"name":"archive","digest":{"sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}]}}}]'
        ;;
      fail)
        json=""
        ;;
    esac
    cat > "$stubs/gh" << EOF
#!/bin/sh
# gh stub: \`gh attestation verify\` answers as the case says; anything else is an error.
[ "\$1" = attestation ] || exit 2
if [ "$3" = fail ]; then
  echo "no attestation" >&2
  exit 1
fi
printf '%s\n' '$json'
EOF
    chmod +x "$stubs/gh"
  fi
}

# Runs install.sh for one case and checks its exit status and its output.
# $1 name, $2 "pass" or "fail" (the exit status expected), $3 text the output must contain,
# $4 gh stub mode (ok, fail, wrong or nogh), $5 VERIFY_ATTESTATION, $6 GH_TOKEN.
case_run() {
  name=$1
  want=$2
  must=$3
  mode=$4
  verify=$5
  token=$6
  dir="$work/$name"
  mkdir -p "$dir/runner"
  if [ "$mode" = nogh ]; then
    make_stubs "$dir" none none
  else
    make_stubs "$dir" gh "$mode"
  fi
  : > "$dir/urls"
  set +e
  output=$(env -i PATH="$dir/stubs:$tools" HOME="$dir" RUNNER_TEMP="$dir/runner" \
    GITHUB_PATH="$dir/github_path" AGENT_VERSION=v0.1.0-alpha.3 \
    VERIFY_ATTESTATION="$verify" GH_TOKEN="$token" \
    "$tools/bash" "$install" 2>&1)
  status=$?
  set -e
  ok=yes
  if [ "$want" = pass ] && [ "$status" -ne 0 ]; then ok=no; fi
  if [ "$want" = fail ] && [ "$status" -eq 0 ]; then ok=no; fi
  case "$output" in
    *"$must"*) ;;
    *) ok=no ;;
  esac
  if [ "$ok" = yes ]; then
    echo "ok   $name"
  else
    failures=$((failures + 1))
    echo "FAIL $name (exit $status)"
    echo "$output" | sed 's/^/     | /'
  fi
  cases=$((cases + 1))
  # The one download is the archive: the checksum file is no longer fetched.
  if [ "$want" = pass ] && [ "$mode" != nogh ]; then
    if [ "$(wc -l < "$dir/urls" | tr -d ' ')" -ne 1 ] || grep -q 'sha256.sum' "$dir/urls"; then
      failures=$((failures + 1))
      echo "FAIL $name: expected one download, the archive, and no sha256.sum"
      sed 's/^/     | /' "$dir/urls"
    fi
  fi
}

case_run verified pass "" ok true tok
case_run failed-verify fail "does not verify" fail true tok
case_run digest-mismatch fail "checksum mismatch" wrong true tok
case_run no-gh fail "gh CLI is not installed" nogh true tok
case_run no-token fail "there is no token" ok true ""
case_run verify-false pass "::warning::build attestation not checked" nogh false ""
case_run bad-value fail "must be true or false" ok maybe tok

echo "test-install: $((cases - failures)) of $cases passed"
if [ "$failures" -ne 0 ]; then
  exit 1
fi
