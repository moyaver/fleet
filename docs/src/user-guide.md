# User guide

## Wrapping Maestro

Put `fleet-agent run --` in front of your `maestro` command:

```bash
fleet-agent run -- maestro test .maestro/
```

The agent never changes what CI sees: Maestro's output is passed through byte for byte, and the step's exit status is Maestro's own, signal deaths included. If Fleet is misconfigured or unreachable, Maestro runs exactly as it would without the agent.

What the agent adds to `maestro test`, only when you haven't set it yourself (turn off with `--fleet-rewrite=off`):

- `--flatten-debug-output --debug-output <run dir>/artifacts`, so the session log and flow bundles land in one place it can follow;
- `--format junit --output <run dir>/report.xml`, the final report it reconciles against;
- `MAESTRO_CLI_LOG_PATTERN_FILE`, a machine-readable log line, plus `MAESTRO_CLI_NO_ANALYTICS=1` and `MAESTRO_DISABLE_UPDATE_CHECK=true`.

It never removes a flag.

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

`FLEET_REPOSITORY`, `FLEET_RUN_ID`, `FLEET_RUN_ATTEMPT`, `FLEET_SHA`, `FLEET_REF` and `FLEET_RUNNER` describe the CI run on systems other than GitHub Actions.

## Reading results: provenance

Every outcome Fleet shows says where it came from. While a run is live, outcomes come from Maestro's log and console (**provisional**). When Maestro writes its report, the report decides (**verified**). An outcome Fleet had to infer, such as a flow cut short by a signal, says **inferred** and is never shown as passed.

## Offline mode and `inspect`

Without `FLEET_SERVER`, everything still happens locally: the run directory gets `journal.jsonl` (every event) and `spool.bin`. To see what Fleet makes of any saved run:

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
