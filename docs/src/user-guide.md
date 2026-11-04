# User guide

## Wrapping Maestro

Put `fleet-agent run --` in front of your `maestro` command:

```bash
fleet-agent run -- maestro test .maestro/
```

The agent never changes what CI sees: Maestro's output is passed through byte for byte, and the step's exit status is Maestro's own, signal deaths included. If Fleet is misconfigured or unreachable, Maestro runs exactly as it would without the agent.

What the agent adds to `maestro test`, only when you haven't set it yourself (turn off with `--fleet-rewrite=off`):

- `--flatten-debug-output --debug-output <run dir>/artifacts`, so the session log and flow bundles land in one place it can follow;
- `--format junit --output <run dir>/report.xml`, the final report it reconciles against, but only when that can't change how Maestro runs (see below);
- `MAESTRO_CLI_LOG_PATTERN_FILE`, a machine-readable log line, plus `MAESTRO_CLI_NO_ANALYTICS=1` and `MAESTRO_DISABLE_UPDATE_CHECK=true`.

It never removes a flag.

**Why the JUnit report is conditional.** In Maestro, `--format` isn't just reporting: asking for a report makes `maestro test` take its multi-flow path, the one it uses for several flows, and that path rejects `-c`/`--continuous`. Fleet must not change how your command runs, so it adds the report only when Maestro would take that path anyway:

| Command | Report added? |
|---|---|
| `maestro test flows/` (a directory) | yes |
| `maestro test a.yaml b.yaml` (several flows), or a planner shard of several flows | yes |
| `maestro test --shard-all N …` with N > 1 | yes |
| `maestro test login.yaml` (one flow) | no |
| any command with `-c` or `--continuous` | no |
| any command with your own `--format` | no (yours is used) |
| a flow path that doesn't exist or can't be read | no |

Without the report, Fleet still follows the run from Maestro's session log, flow bundles and exit status; it just has no JUnit to reconcile against. Exit status 1 still counts as test failures when Fleet saw a flow fail in Maestro's output or flow bundles; if it saw none, the exit is classified as unknown (`other`) rather than as a Maestro error, which is reserved for a report that was expected and is missing. The agent logs its decision once, at info level (`junit report decision`).

## Configuration

| Variable | Meaning |
|---|---|
| `FLEET_SERVER` | Fleet server base URL (`https://…`). Unset: offline mode. |
| `FLEET_TOKEN` | API key for the project. |
| `FLEET_JOB_KEY` | Stable job identity. On GitHub use `${{ job.check_run_id }}`. Otherwise `GITHUB_JOB` plus a hash of `FLEET_MATRIX` is used. |
| `FLEET_RUN_DIR` | The agent's directory for this run (journal, spool, its own log). |
| `FLEET_GRACE` | Seconds between the first INT/TERM and killing Maestro's process group (default 15). |
| `FLEET_FINALIZE_TIMEOUT` | Seconds the agent may spend finishing after Maestro exits (default 60). |
| `FLEET_FLUSH_TIMEOUT` | Seconds the observer waits for the server to ack everything (default 20). |
| `FLEET_UPLOAD_ARTIFACTS` | `off` stops the agent uploading evidence (failure screenshots, `commands.json`, the JUnit report, `maestro.log`). On by default when `FLEET_SERVER` is set. |
| `FLEET_UPLOAD_TIMEOUT` | Seconds the agent waits at the end of the run for uploads to finish (default: half of `FLEET_FLUSH_TIMEOUT`). Unfinished uploads are abandoned. Uploads and the final flush share the `FLEET_FLUSH_TIMEOUT` budget. |
| `FLEET_UPLOAD_RATE` | Upload rate cap in bytes per second, shared by all uploads (default 4 MiB/s). |

### What the agent uploads

At most two files at a time, hashed while they stream. Uploading never changes Maestro's result or output: a refused, failed or timed-out upload is recorded in the run's journal (`artifact.rejected`) and nothing else. **Text is redacted before it leaves the runner**: `commands.json`, the JUnit report and `maestro.log` go through the same redaction as the console (`-e` values, `MAESTRO_*` values, secret-named variables, known token shapes). A text that is not valid UTF-8, contains NUL bytes or is over 16 MiB cannot be redacted reliably, so it is **not uploaded**, and the run's journal says so (`artifact_not_uploaded`). Screenshots are binary: they are never redacted, only access-controlled on the server.

`FLEET_REPOSITORY`, `FLEET_RUN_ID`, `FLEET_RUN_ATTEMPT`, `FLEET_SHA`, `FLEET_REF` and `FLEET_RUNNER` describe the CI run on systems other than GitHub Actions.

## Reading results: provenance

Every outcome Fleet shows says where it came from. While a run is live, outcomes come from Maestro's log and console (**provisional**). When Maestro writes its report, the report decides (**verified**). An outcome Fleet had to infer, such as a flow cut short by a signal, says **inferred** and is never shown as passed.

## Offline mode and `inspect`

Without `FLEET_SERVER`, everything still happens locally: the run directory gets `journal.jsonl` (every event) and `spool.bin` (capped at 64 MiB; if a run overflows it, the spool ends in a `journal.gap` saying which events it does not hold). To see what Fleet makes of any saved run:

```bash
fleet-agent inspect "$FLEET_RUN_DIR/artifacts"
```

`--json` prints the result as JSON; `--json --events` adds every journal event.

## When the agent dies: `flush`

If the runner kills the agent before everything reached the server, an `if: always()` step sends the spool:

```bash
fleet-agent flush "$FLEET_RUN_DIR/spool.bin"
```

The server ignores events it already has, so flushing twice is safe.

## Fleet Planner shards

To run one shard of a [Fleet plan](https://github.com/moyaver/fleet/blob/main/docs/decisions/0013-fleet-planner.md), give the manifest and the shard. The shard's flows are the only flows the command runs, so don't name any flow paths yourself:

```bash
fleet-agent run --plan plan.json --shard 2 -- maestro test
```

A plan that can't be read stops the step (exit 2) rather than running the whole suite on every shard. So does a command that already names flow files or directories (`maestro test .maestro/`): Maestro would run those too. Maestro options such as `--device` and `-e` are fine.

## Example workflow

See [`examples/github-actions/fleet-agent.yml`](https://github.com/moyaver/fleet/blob/main/examples/github-actions/fleet-agent.yml).
