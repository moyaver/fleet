# `maestro test` options at Maestro CLI v2.12.1

Rebuilt by reading the picocli annotations in the Maestro source. Nothing was executed (`maestro test --help` was never run), so the help text Maestro actually renders can differ in wrapping and in how it substitutes `${DEFAULT-VALUE}`.

- Tag: `v2.12.1` (commit `67d8a677029261fa424a45a10d08eacd6aa0deed`, 2026-11-02), `maestro-cli/gradle.properties` reads `CLI_VERSION=2.12.1`
- Command class: `maestro-cli/src/main/java/maestro/cli/command/TestCommand.kt`, lines 81-232
- Mixins: `maestro-cli/src/main/java/maestro/cli/DisableAnsiMixin.kt` and `maestro-cli/src/main/java/maestro/cli/ShowHelpMixin.kt`
- Global (parent) options: `maestro-cli/src/main/java/maestro/cli/App.kt`, lines 81-111. They go before `test` (for example `maestro --verbose test ...`).

`@Command(name = "test", description = ["Test a Flow or set of Flows on a local iOS Simulator or Android Emulator"])`

## Positional parameters

| Name | Kotlin type | Arity | Description (verbatim) | Default |
|---|---|---|---|---|
| `flowFiles` | `Set<File>` | `1..*` | "One or more flow files or folders containing flow files" | none (required) |

## `test` options

The Description column is copied from the source. A dash means the option has no `description` attribute. "Field default" is the Kotlin initialiser; picocli keeps it when the option is not passed.

| Option name(s) | Kotlin type | Description (verbatim) | Field default | Notes from source |
|---|---|---|---|---|
| `--config` | `File?` | "Optional YAML configuration file for the workspace. If not provided, Maestro will look for a config.yaml file in the workspace's root directory." | `null` | Exits with `CliError` if the file does not exist (L266-268). Auto-discovery checks `config.yaml` then `config.yml` in each input directory (`WorkspaceExecutionPlanner.findConfigFile`). |
| `-s`, `--shards` | `Int?` | "Number of parallel shards to distribute tests across" | `null` | `@Deprecated("Use --shard-split or --shard-all instead")`. Prints a deprecation warning and is copied into `shardSplit` (L260-264). |
| `--shard-split` | `Int?` | "Run the tests across N connected devices, splitting the tests evenly across them" | `null` | Cannot be combined with `--shard-all` (L256-258). Flows are assigned round-robin by index (`index % N`, L655-662). The shard count is capped at the flow count (L403). |
| `--shard-all` | `Int?` | "Run all the tests across N connected devices" | `null` | Every shard gets a full copy of the plan (L655). The shard count is capped at the device count (L405). |
| `-c`, `--continuous` | `Boolean` | - | `false` | Watch mode for a single flow only. Exits with an error for multiple flows or when `--format` is set (L495-501). |
| `-e`, `--env` | `Map<String,String>` | - | `emptyMap()` | Repeatable `-e KEY=VALUE`. Values are injected as a `DefineVariablesCommand`, which is serialized into `commands.json`. |
| `--format` | `ReportFormat` (converter `ReportFormat.Converter`) | "Test report format (default=${DEFAULT-VALUE}): ${COMPLETION-CANDIDATES}" | `ReportFormat.NOOP` | Values: `JUNIT` (`.xml`), `HTML` (`.html`), `HTML_DETAILED` / `HTML-DETAILED` (`.html`, adds steps), `NOOP`. Case-insensitive. Any value other than NOOP forces the multi-flow "suite" path even for a single flow (L494-495). |
| `--test-suite-name` | `String?` | "Test suite name" | `null` | Becomes the JUnit `<testsuite name>`. Falls back to `"Test Suite"`. |
| `--output` | `File?` | - | `null` | Report destination. When omitted: `report.xml` / `report.html` in the CWD (L694-695). Written once, after every shard has finished. |
| `--debug-output` | `String?` | "Configures the debug output in this path, instead of default" | `null` | Root for `maestro.log` (and for the bundles when `--test-output-dir` is not set). Without `--flatten-debug-output` the path becomes `<path>/.maestro/tests/<yyyy-MM-dd_HHmmss>/`. See `TestDebugReporter.kt` L187-226. |
| `--test-output-dir` | `String?` | "Directory for this run's artifacts — manifest.json, commands.json, logs/, takeScreenshot/, and startRecording/ written directly into it (overrides the default output location)" | `null` | The code actually writes to `<dir>/<yyyy-MM-dd_HHmmss>/<flow>/...`, not "directly into" the directory. Ignored when `--flatten-debug-output` is set. Falls back to `testOutputDir` in `config.yaml`. |
| `--flatten-debug-output` | `Boolean` | "All file outputs from the test case are created in the folder without subfolders or timestamps for each run. It can be used with --debug-output. Useful for CI." | `false` | Output root becomes `--debug-output`, or `user.home` when `--debug-output` is not given. Turns off the 14-day cleanup of old runs (`deleteOldFiles`). |
| `--include-tags` | `List<String>` (split `,`) | "List of tags that will remove the Flows that does not have the provided tags" | `emptyList()` | Merged with `includeTags` from `config.yaml`. The match is "any tag". |
| `--exclude-tags` | `List<String>` (split `,`) | "List of tags that will remove the Flows containing the provided tags" | `emptyList()` | Merged with `excludeTags` from `config.yaml`. |
| `--headless` | `Boolean` | "(Web only) Run the tests in headless mode" | `false` | |
| `--screen-size` | `String?` | "(Web only) Set the size of the headless browser. Use the format {Width}x{Height}. Usage is --screen-size 1920x1080" | `null` | Checked against the regex `\d+x\d+`. |
| `--analyze` | `Boolean` | "[Beta] Enhance the test output analysis with AI Insights" | `false` | Also sets `captureFullArtifacts=true`, which writes a screenshot before every step plus a full-run recording. Runs a cloud AI analysis at the end. |
| `--api-url` | `String` | "[Beta] API base URL" | `"https://api.copilot.mobile.dev"` | |
| `--api-key` | `String?` | "[Beta] API key" | `null` | |
| `--reinstall-driver` / `--no-reinstall-driver` | `Boolean` (negatable, `defaultValue="true"`, `fallbackValue="true"`) | "Reinstalls driver before running the test. On iOS, reinstalls xctestrunner driver. On Android, reinstalls both driver and server apps. Set to false to skip reinstallation." | `true` | When `true`, `AndroidDriver.close()` also uninstalls the driver apps at shutdown. |
| `--apple-team-id` | `String?` | "The Team ID is a unique 10-character string generated by Apple that is assigned to your team's Apple account." | `null` | `hidden = true` |
| `-p`, `--platform` | `String?` | "Select a platform to run on" | `null` | Filters the auto-selected devices only when no `--device` is given (L385-390). |
| `--device`, `--udid` | `String?` | "Device ID to run on explicitly, can be a comma separated list of IDs: --device \"Emulator_1,Emulator_2\" " | `null` | Comma-separated list. Shard *i* uses the *i*-th ID (L472). Every ID must already be connected, otherwise `CliError "Device X was requested, but it is not connected."` |
| `--driver-host-port` | `Int?` | - | `null` | `hidden = true`. When unset, each shard gets an OS-assigned free port (`ServerSocket(0)`, L534-545). |

### Mixin options available on `test`

| Option name(s) | Type | Description (verbatim) | Default | Source |
|---|---|---|---|---|
| `--no-color`, `--no-ansi` (negatable: `--color`, `--ansi`) | `Boolean` `enableANSIOutput` | "Enable / disable colors and ansi output" | `true` (field). When the option is not passed, the effective value is `isatty(stdout)`. | `DisableAnsiMixin.kt` L12-17, L41-57 |
| `-h`, `--help` | `Boolean` (`usageHelp`) | "Display help message" | `false` | `ShowHelpMixin.kt` |

## Global options (`App`, given before `test`)

| Option name(s) | Type | Description (verbatim) | Default | Notes |
|---|---|---|---|---|
| `-v`, `--version` | `Boolean?` (`versionHelp`) | "Display CLI version" | `false` | |
| `-p`, `--platform` | `String?` | "(Optional) Select a platform to run on" | `null` | Used when `test -p` is not set. |
| `--host` | `String?` | - | `null` | `hidden = true` |
| `--port` | `Int?` | - | `null` | `hidden = true` |
| `--driver-host-port` | `Int?` | - | `null` | `hidden = true` |
| `--device`, `--udid` | `String?` | "(Optional) Device ID to run on explicitly, can be a comma separated list of IDs: --device \"Emulator_1,Emulator_2\" " | `null` | Used when `test --device` is not set. |
| `--verbose` | `Boolean` | "Enable verbose logging" | `false` | Adds a log4j console appender to stdout. Default pattern `%highlight{[%5level]} %msg%n`, which `MAESTRO_CLI_LOG_PATTERN_CONSOLE` overrides. |
| `--no-color`/`--no-ansi`, `-h`/`--help` | (mixins) | as above | | |

## Options that do not exist (checked)

- No `--retries`, `--retry` or other flow-level retry option, and no `--dry-run` or list-flows option.
- No `--json`, event-stream or reporter-plugin option.
- No timeout option on `test`. The driver start-up timeout comes from the `MAESTRO_DRIVER_STARTUP_TIMEOUT` environment variable, according to the docs.

## Environment variables that change `maestro test` behaviour (source refs)

| Variable | Effect | Source |
|---|---|---|
| `MAESTRO_CLI_NO_ANALYTICS` (any value) | Turns off PostHog analytics events | `analytics/Analytics.kt` L20, L38-39 |
| `MAESTRO_DISABLE_UPDATE_CHECK=true` | Suppresses the update banner printed to stderr at exit. The async fetch at start-up still runs (`Updates.fetchUpdatesAsync()` is not gated). | `update/Updates.kt`, `App.kt` L145, L189 |
| `MAESTRO_CLI_LOG_PATTERN_FILE` | log4j PatternLayout for the session `maestro.log`. Does not apply to the per-flow `logs/maestro.log`. | `maestro-client/.../debuglog/LogConfig.kt` L14 |
| `MAESTRO_CLI_LOG_PATTERN_CONSOLE` | log4j PatternLayout for `--verbose` console output | `LogConfig.kt` L15 |
| `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED` | Suppresses the "Analyze with Ai" box printed at exit | `insights/TestAnalysisManager.kt` |
| `XDG_STATE_HOME` | Default output root moves to `$XDG_STATE_HOME/maestro/tests/<ts>` (otherwise `~/.maestro/tests/<ts>`) | `util/EnvUtils.kt` L36-42 |
| `MDEV_CI`, `CI`, `GITHUB_ACTIONS`, `GITLAB_CI`, `JENKINS_HOME`, `BITRISE_IO`, `CIRCLECI`, `BUILDKITE`, `TF_BUILD`, ... | CI detection. Only hides the cloud promotion boxes and is logged or sent as analytics. It does **not** change ANSI or console format. | `util/CiUtils.kt` |
| `MAESTRO_*` (any) | Injected into every flow as a variable, except `MAESTRO_SHARD_ID` and `MAESTRO_SHARD_INDEX`, which Maestro sets itself. Also serialized into `commands.json`. | `maestro-orchestra-models/.../util/Env.kt` L102-132 |
| `MAESTRO_OPTS` / `JAVA_OPTS` | Standard Gradle start-script JVM options. This is inferred from the Gradle `application` plugin template; the template itself was not read. | `maestro-cli/build.gradle.kts` L15-36, L138-160 |
