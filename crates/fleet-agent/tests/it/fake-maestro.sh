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
bundles() {
  for d in "$FIX"/run/debug/*/; do
    [ -d "$d" ] && cp -R "${d%/}" "$DEBUG/"
  done
}
# Without a gate the bundles land with the log; with one they wait for the gate (below).
[ -z "$FLEET_FAKE_GATE" ] && bundles
# With FLEET_FAKE_GATE=<file>, print the console output now (the live outcomes), then wait until
# that file exists (up to FLEET_FAKE_GATE_TIMEOUT seconds, default 120) before writing the flow
# bundles (which, like the report, settle outcomes as verified) and the report, and exiting: a
# test can look at the open run, every outcome provisional, without racing the clock. Otherwise
# linger, then print.
if [ -n "$FLEET_FAKE_GATE" ]; then
  cat "$FIX/stdout.bin"
  tries=$(( ${FLEET_FAKE_GATE_TIMEOUT:-120} * 20 ))
  while [ ! -e "$FLEET_FAKE_GATE" ] && [ "$tries" -gt 0 ]; do
    sleep 0.05
    tries=$((tries - 1))
  done
  bundles
else
  sleep "${FLEET_FAKE_LINGER:-0.6}"
  cat "$FIX/stdout.bin"
fi
[ -n "$OUT" ] && [ -f "$FIX/run/report.xml" ] && cp "$FIX/run/report.xml" "$OUT"
code=$(sed -n 's/.*"code": *\([0-9][0-9]*\).*/\1/p' "$FIX/exit.json")
exit "${code:-1}"
