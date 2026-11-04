#!/bin/sh
# A stand-in for `maestro` in agent tests: replays the captured fixture in $FLEET_FAKE_FIXTURE.
# It honours --debug-output and --output the way the real CLI does, writes the session log line by
# line (so the agent tails it), then the flow bundles, prints the captured stdout and exits with
# the captured code.
FIX="$FLEET_FAKE_FIXTURE"
# Record the argv exactly as received, one argument per line, so tests can check what Maestro got.
[ -n "$FLEET_FAKE_ARGV_LOG" ] && printf '%s\n' "$@" > "$FLEET_FAKE_ARGV_LOG"
DEBUG=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --debug-output) DEBUG="$2"; shift ;;
    --output) OUT="$2"; shift ;;
  esac
  shift
done
mkdir -p "$DEBUG"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$DEBUG/maestro.log"
done < "$FIX/run/debug/maestro.log"
for d in "$FIX"/run/debug/*/; do
  [ -d "$d" ] && cp -R "${d%/}" "$DEBUG/"
done
sleep "${FLEET_FAKE_LINGER:-0.6}"
cat "$FIX/stdout.bin"
[ -n "$OUT" ] && [ -f "$FIX/run/report.xml" ] && cp "$FIX/run/report.xml" "$OUT"
code=$(sed -n 's/.*"code": *\([0-9][0-9]*\).*/\1/p' "$FIX/exit.json")
exit "${code:-1}"
