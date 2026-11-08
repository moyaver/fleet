#!/bin/sh
# A stand-in for `maestro` in agent tests: replays the captured fixture in $FLEET_FAKE_FIXTURE.
# It honours --debug-output, --test-output-dir, --flatten-debug-output and --output the way the real
# CLI does, writes the session log line by line (so the agent tails it), then the flow bundles,
# prints the captured stdout and exits with the captured code.
FIX="$FLEET_FAKE_FIXTURE"
# Record the argv exactly as received, one argument per line, so tests can check what Maestro got.
[ -n "$FLEET_FAKE_ARGV_LOG" ] && printf '%s\n' "$@" > "$FLEET_FAKE_ARGV_LOG"
DEBUG=""
TESTDIR=""
FLAT=""
OUT=""
FORMAT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --debug-output) DEBUG="$2"; shift ;;
    --test-output-dir) TESTDIR="$2"; shift ;;
    --flatten-debug-output) FLAT=1 ;;
    --output) OUT="$2"; shift ;;
    --format) FORMAT="$2"; shift ;;
  esac
  shift
done
# Where the real CLI puts the session log and the flow bundles (research 01 §4.1, `[V]`; the
# captured trees in fixtures/maestro/2.12.1 are the runtime proof for paths-*, html-*). With
# --flatten-debug-output both go into --debug-output, or into the home directory without one, and
# --test-output-dir is ignored. Otherwise the log goes into a `<ts>` directory under
# `<debug>/.maestro/tests` (or the state directory), and the bundles into `<test>/<ts>` when
# --test-output-dir is given, else beside the log.
if [ -n "$FLAT" ]; then
  LOGDIR="${DEBUG:-$HOME}"
  BUNDLEDIR="$LOGDIR"
else
  STAMP=$(date +%Y-%m-%d_%H%M%S)
  if [ -n "$DEBUG" ]; then
    LOGBASE="$DEBUG/.maestro/tests"
  else
    STATE="${XDG_STATE_HOME:+$XDG_STATE_HOME/maestro}"
    LOGBASE="${STATE:-$HOME/.maestro}/tests"
  fi
  LOGDIR="$LOGBASE/$STAMP"
  if [ -n "$TESTDIR" ]; then BUNDLEDIR="$TESTDIR/$STAMP"; else BUNDLEDIR="$LOGDIR"; fi
fi
mkdir -p "$LOGDIR" "$BUNDLEDIR"
# The version line is the fixture's, unless FLEET_FAKE_VERSION names another version (or is empty:
# then Maestro logs no version at all).
while IFS= read -r line; do
  case "$line" in
    *"Maestro Version: "*)
      if [ -z "${FLEET_FAKE_VERSION+x}" ]; then
        printf '%s\n' "$line"
      elif [ -n "$FLEET_FAKE_VERSION" ]; then
        printf '%s\n' "${line%Maestro Version: *}Maestro Version: $FLEET_FAKE_VERSION"
      fi ;;
    *) printf '%s\n' "$line" ;;
  esac
done < "$FIX/run/debug/maestro.log" >> "$LOGDIR/maestro.log"
bundles() {
  for d in "$FIX"/run/debug/*/; do
    [ -d "$d" ] && cp -R "${d%/}" "$BUNDLEDIR/"
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
# An HTML format (`html`, `html-detailed`) writes its report where `--output` says, or to
# `report.html` in the working directory without one, as Maestro does
# (fixtures/maestro/2.12.1/html-report-default-path). The report is the test's $FLEET_FAKE_HTML when
# a test sets it, else the fixture's own run/report.html. Either is written now, as Maestro writes
# it, so it counts as this run's; a test that needs a stale report sets FLEET_FAKE_HTML_KEEP_MTIME,
# and the copy keeps the source's modification time. Otherwise the fixture's JUnit report is written
# to `--output`.
case "$FORMAT" in
  html|html-detailed)
    target="${OUT:-report.html}"
    if [ -n "$FLEET_FAKE_HTML" ]; then
      if [ -n "$FLEET_FAKE_HTML_KEEP_MTIME" ]; then
        cp -p "$FLEET_FAKE_HTML" "$target"
      else
        cp "$FLEET_FAKE_HTML" "$target"
      fi
    elif [ -f "$FIX/run/report.html" ]; then
      cp "$FIX/run/report.html" "$target"
    fi ;;
  *)
    if [ -n "$OUT" ] && [ -f "$FIX/run/report.xml" ]; then
      cp "$FIX/run/report.xml" "$OUT"
    fi ;;
esac
code=$(sed -n 's/.*"code": *\([0-9][0-9]*\).*/\1/p' "$FIX/exit.json")
exit "${code:-1}"
