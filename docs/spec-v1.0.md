# Maestro Fleet
## Technical Product & Architecture Specification — v1.0

> Original specification as supplied, preserved verbatim for reference.
> Research findings and proposed revisions (including the move to an all-Rust
> implementation) live in [`../research/`](../research/) and [`./decisions/`](./decisions/).

**Product type:** Internal developer tooling / CI test observability  
**Deployment:** Self-hosted, infrastructure-independent  
**Initial target:** Android emulators + Maestro CLI  
**Future targets:** iOS simulators, additional CI providers, physical devices  
**Execution model:** CI-driven, agent-observed, browser-monitored

---

## 1. Product vision

Maestro Fleet is a centralized observability platform for automated mobile tests.

It connects test execution, machine health, emulator health, debugging artifacts, and historical quality metrics into one web application.

It must answer:

- What is running right now?
- Which CI job, worker and device is executing each test?
- How many test executions have passed, failed or remain?
- Is a runner overloaded, thermally constrained, disconnected or stalled?
- Why did a test fail, and what evidence is available?
- Is a particular flow becoming slower or less reliable?
- Did a code change introduce regressions?
- How efficiently are we using our CI capacity?

Fleet is not a test execution engine, CI scheduler or replacement for Maestro.

**Core principle:** Maestro remains responsible for testing. CI remains responsible for orchestration. Fleet is responsible for visibility and analysis.

## 2. System architecture

The platform has four components.

### A. Fleet Agent

A lightweight command-line application installed inside the CI environment.

Responsibilities:

- Launch and supervise the Maestro process.
- Collect test progress from available live signals.
- Monitor host and emulator metrics.
- Identify the CI execution context.
- Publish events over an outbound WebSocket.
- Reconcile completed reports with live observations.
- Upload debugging artifacts.
- Preserve the original Maestro exit result.

The agent runs only for the lifetime of the CI job.

No permanent daemon or privileged agent is required.

### B. Fleet Server

A persistent backend providing:

- WebSocket ingestion
- Authentication and authorization
- Live state aggregation
- Test result normalization
- History storage
- Artifact metadata and download access
- Metrics and analytics APIs
- Dashboard update broadcasts

It must not have access to CI runner shells or require inbound connections to CI machines.

### C. Fleet Web

A browser-based dashboard for developers, QA engineers and engineering leads.

It presents current executions, health data, failure investigations, historical trends and downloadable reports.

### D. Storage

Two logical storage systems:

**Relational database:** run metadata, events, normalized results, device configurations, sampled metrics and historical statistics.

**Object storage:** screenshots, videos, complete reports and larger debugging files.

The storage interfaces must not depend on a specific cloud provider.

### Data flow

CI pipeline  
→ Fleet Agent starts  
→ Agent establishes authenticated WebSocket  
→ Agent launches Maestro  
→ Metrics and test events stream to Fleet Server  
→ Server stores and broadcasts changes  
→ Browser displays live updates  
→ Maestro finishes  
→ Agent uploads and reconciles final artifacts  
→ Server finalizes results and retains history

The browser never communicates directly with CI runners.

---

## 3. Execution hierarchy

Define these entities explicitly.

| Entity | Meaning |
|---|---|
| Project | An application or test repository |
| Pipeline Run | One CI workflow execution |
| Job | One CI job within the workflow |
| Worker | The machine or runtime that executes the job |
| Device | An emulator, simulator or physical device |
| Suite Execution | One invocation of Maestro |
| Shard | A subset or replication of the suite assigned to a device |
| Flow Execution | One actual execution of a top-level Maestro flow |
| Step | A command or nested action within a flow |
| Attempt | A distinct attempt when execution is retried |
| Artifact | A log, report, screenshot or recording |

Example:

Pipeline #142  
→ Job: Android Regression  
→ Worker: mac-mini-01  
→ Suite: KYC  
→ Shard: 2  
→ Device: emulator-5556  
→ Flow: KYC Address Verification  
→ Attempt: 1  
→ Step: assertVisible

**Do not assume one job equals one emulator, or one Maestro process equals one shard.**

The model must support multiple processes, devices and shards on the same worker.

A test repeated across devices creates separate flow executions, not one shared result.

## 4. Fleet Agent design

### Runtime and packaging

Recommended initial implementation: Python 3.11+.

Suggested dependencies:

- `psutil` for portable process and resource metrics.
- `websockets` for transport.
- Python standard library for subprocess management, artifact processing and local buffering.

Package it as a versioned command-line tool.

An optional standalone executable can be produced if installing Python dependencies on CI runners is undesirable.

### Main interface

```bash
maestro-fleet run \
  --suite regression \
  -- \
  maestro test \
    --format junit \
    --output build/fleet/report.xml \
    --test-output-dir build/fleet/artifacts \
    --debug-output build/fleet/artifacts \
    .maestro/
```

The agent must:

1. Read CI metadata and configuration.
2. Allocate stable execution identifiers.
3. Connect to Fleet Server.
4. Create and validate artifact directories.
5. Start the supplied Maestro command.
6. Read standard output and standard error continuously.
7. Collect metrics on an independent schedule.
8. Detect lifecycle events and available artifacts.
9. Handle termination signals.
10. Reconcile final reports.
11. Upload allowed artifacts.
12. Exit with Maestro's original exit status.

Never turn a failing Maestro result into a successful CI job.

### Operational commands

```text
maestro-fleet run
maestro-fleet check
maestro-fleet inspect
maestro-fleet version
maestro-fleet selftest
```

`check` validates connectivity, authentication, platform capabilities and configured output paths without running tests.

`inspect` analyzes previously saved Maestro results offline.

### Metrics collection

Sampling interval: configurable, default 5 seconds.

**Host:**
- CPU utilization and logical core count
- Total and available memory
- Memory pressure where supported
- Swap usage
- System load
- Host uptime
- Thermal state and source
- CPU temperature only when an actual sensor is available

**Processes:**
- Maestro process CPU and memory usage
- Emulator process CPU and memory usage where attributable
- Process existence and runtime

**Devices:**
- Emulator/device serial
- AVD or simulator name
- Android API level or iOS runtime version
- ADB or simulator connection state
- Boot readiness
- Assigned suite/shard
- Current activity
- Optional device memory and CPU readings, explicitly labeled as guest metrics

Measurements must carry units, timestamps and availability indicators.

Do not mix host CPU utilization with emulator guest CPU utilization.

For macOS, use a supported system thermal-state interface where available. Never invent an exact CPU temperature.

### Progress collection

Use a hybrid evidence model:

**During execution:**
- Maestro process lifecycle
- Parsed live console events
- Session debug logs
- Known shard/device information
- Completed-flow artifacts as they appear

**After execution:**
- JUnit report
- Maestro command artifacts
- Detailed HTML reports when generated
- Process exit code
- Final artifact inventory

Live parsing must be version-aware and tolerant of changes to console formatting.

The final structured report overrides provisional live states where appropriate.

### Accurate progress accounting

Distinguish:

- Planned flow executions
- Started flow executions
- Completed flow executions
- Passed
- Failed
- Skipped
- Interrupted
- Unknown

A nested `runFlow` is a step/subflow, not automatically a separate top-level test.

A command retry is not automatically a new test attempt.

`--shard-all` and `--shard-split` must have different accounting semantics.

An exact completion percentage is shown only when the execution plan is known.

If the plan is uncertain, show verified observed counts and an indeterminate progress indicator.

Provide an optional suite manifest or explicit flow selection from CI for reliable planned totals. Do not assume scanning YAML files exactly reproduces Maestro's own discovery and filtering behavior.

### Resource overhead requirements

The agent must be lightweight enough not to meaningfully distort test performance.

Design targets, to be validated experimentally:

- Under 1% average host CPU on an idle collector
- Minimal bounded resident memory
- No unbounded log accumulation
- No expensive global process scans every second
- No unnecessary ADB commands
- No blocking network operations in the Maestro output-reading path

Metrics collection may be degraded or throttled automatically if the host is under severe load.

---

## 5. Live transport protocol

### Primary transport

Authenticated WebSocket over TLS.

Each job establishes one connection:

```text
wss://fleet.company.example/api/v1/agent/stream
```

Multiple devices and Maestro processes share the connection, identified within each event.

The connection must not require any inbound network access to the runner.

### Event types

```text
session.started
worker.registered
worker.metrics
device.discovered
device.status_changed
suite.started
flow.started
step.started
step.completed
flow.completed
flow.failed
suite.completed
artifact.available
session.completed
session.interrupted
agent.warning
```

### Example event

```json
{
  "schema_version": 1,
  "event_id": "evt-000128",
  "sequence": 128,
  "type": "flow.failed",
  "occurred_at": "2026-11-03T21:15:00Z",
  "pipeline_id": "pipeline-142",
  "job_id": "android-regression-1",
  "execution_id": "exec-abc123",
  "device_id": "emulator-5554",
  "flow_id": "kyc/address-verification.yaml",
  "attempt": 1,
  "payload": {
    "step": "assertVisible",
    "message": "Expected element was not visible",
    "duration_ms": 28420,
    "evidence": "live"
  }
}
```

### Reliability

Each event receives a monotonically increasing sequence number within its agent session.

The server acknowledges durable ingestion.

The agent maintains a bounded local queue of unacknowledged events.

On reconnect:

1. Authenticate again.
2. Resume the existing session.
3. Report the last acknowledged sequence.
4. Replay outstanding events.
5. Receive the current authoritative state.

Server operations must be idempotent.

Duplicate events must not create duplicate test executions or inflate pass/fail counts.

When buffering is full, discard or coalesce low-priority metric samples first. Preserve lifecycle events whenever possible.

An interrupted connection must never terminate Maestro.

A permanently lost CI runner may still lose unacknowledged data. The backend must identify missing finalization rather than assume success.

### Update frequencies

| Information | Frequency |
|---|---|
| CPU/memory/device health | Every 5 seconds |
| Flow start/end | Immediately when detected |
| Step changes | Immediately when detected |
| Keepalive | As required by connection policy |
| Durable metrics checkpoint | Approximately 30–60 seconds |
| Final report | Once available |

Actual browser update frequency should be independent of how frequently metrics are retained historically.

---

## 6. Fleet Server

### Responsibilities

The server accepts telemetry, builds authoritative execution state, persists history and distributes live updates.

### Proposed deployment units

**Application service:** APIs, ingestion, live WebSocket connections and business logic.

**PostgreSQL:** durable operational data.

**S3-compatible object storage:** large artifacts.

**Reverse proxy / ingress:** HTTPS, WebSocket upgrades and access control integration.

An optional Redis service can be added for cross-instance event fan-out if the backend becomes horizontally scaled.

One application instance is sufficient for a small initial deployment, assuming acceptable internal availability requirements.

### Backend stack recommendation

- TypeScript on a supported Node.js LTS release
- Fastify or an equivalent HTTP framework
- WebSocket server integration
- PostgreSQL and versioned migrations
- Typed runtime validation of incoming events
- OpenAPI documentation for REST endpoints

Keep backend contracts independent of the frontend framework.

### REST API

Suggested API groups:

```text
/api/v1/projects
/api/v1/pipelines
/api/v1/jobs
/api/v1/workers
/api/v1/devices
/api/v1/executions
/api/v1/flows
/api/v1/artifacts
/api/v1/analytics
/api/v1/health
```

Use REST for history, browsing, reports and initial dashboard state.

Use WebSockets for live changes.

Large artifacts must use HTTPS uploads, not WebSocket messages.

### Live state

Maintain a current materialized view of each active pipeline, job, worker, device and flow execution.

On browser connection:

1. Authenticate user.
2. Fetch initial authorized snapshot.
3. Subscribe to scoped live updates.
4. Reconcile updates using version numbers or event sequences.
5. Recover with a fresh snapshot after a connection gap.

Avoid using individual telemetry events as the only source of reconstructing current state on the browser.

### State expiration

Define explicit liveness and execution states.

**Worker:** online, stale, disconnected, completed.

**Job:** pending, starting, running, completed, failed, cancelled, interrupted, unknown.

**Flow:** pending, running, passed, failed, skipped, interrupted, unknown.

If heartbeats stop, mark the worker disconnected, not idle.

If the CI job disappears without finalization, eventually classify it as interrupted or unknown based on available evidence.

Do not invent successful test outcomes.

---

## 7. Storage and data model

Initial database tables:

```text
projects
pipelines
jobs
workers
devices
suite_executions
shards
flow_definitions
flow_executions
flow_attempts
step_events
telemetry_samples
artifacts
agent_sessions
ingestion_cursors
audit_events
```

### Identity and grouping

A pipeline ID must distinguish different execution attempts of the same workflow.

A job ID must remain stable during reconnection.

Flow identity should include a normalized repository-relative path and relevant platform/configuration dimensions.

Each flow execution also requires a unique ID.

Do not use flow display names as database primary keys.

### Metrics retention

Keep detailed live metrics while a run is active.

Persist downsampled historical metrics instead of every raw sample indefinitely.

Recommended initial policy, subject to company approval:

- 5-second readings during live execution
- 1-minute historical metrics
- 90 days of detailed historical metrics
- 1 year of aggregate statistics
- Configurable deletion of old records

### Artifact storage

Organize artifacts by project, pipeline, job and execution.

Store metadata including:

- Artifact type
- Content type
- Size
- SHA-256 checksum
- Creation time
- Associated flow/step
- Storage reference
- Retention class

Use checksums to detect corruption and optionally deduplicate identical content.

Do not automatically publish every file in Maestro's output directory. Apply a bounded allowlist of accepted artifact types.

Default artifact policy:

- JUnit XML: retain for historical reporting
- Debug logs: retain for a limited period
- Failure screenshots: retain longer
- Successful-run screenshots: short retention
- Videos: optional and size-limited
- Entire debug bundles: downloadable where authorized

Support exports without requiring direct storage credentials in the browser.

---

## 8. Web dashboard

Recommended frontend:

- React + TypeScript
- Vite
- Tailwind CSS
- Accessible component library
- Lightweight charting library
- WebSocket-based live state updates

Visual direction: polished engineering dashboard, neutral colors, restrained status indicators, high information density, excellent readability.

### Screen A — Fleet Overview

Landing page.

Displays:

**Summary**
- Active pipeline runs
- Running jobs
- Active emulators
- Passed and failed flow executions
- Current failures requiring attention
- Average runner utilization

**Live execution list**
- Project
- Branch/commit
- Triggering CI workflow
- Status
- Completed/planned tests
- Passed/failed
- Elapsed time
- Link to originating CI job

**Runner health**
- CPU
- Memory
- Thermal condition
- Emulator count
- Current workload
- Data freshness

**Recent activity**
- New test failures
- Completed pipelines
- Disconnected runners
- Regressions

### Screen B — Pipeline Detail

Shows one complete CI workflow.

Header:

- Project and pipeline ID
- Commit and branch
- Trigger
- Start/end time
- Final CI result
- Maestro result
- Total duration

Main sections:

**Test progress:** completed, running, pending, failed, skipped.

**Jobs and devices:** hierarchical view of every worker and its active devices.

**Execution timeline:** a Gantt-style visualization showing when each device executed each flow.

**Live activity:** meaningful event stream, updated continuously.

**Failures:** immediately visible and navigable.

### Screen C — Worker and Device Detail

Shows a selected worker or emulator.

**Machine health**
- CPU history
- Memory history
- Swap history
- Thermal state
- Maestro/emulator process usage

**Device status**
- Model / AVD
- OS version
- Boot status
- Connection status
- Current shard
- Assigned tests

**Execution details**
- Current flow
- Current step
- Flow duration
- Time since last progress event
- Previous flow outcomes

Correlate metric spikes with the flow execution timeline.

### Screen D — Flow Investigation

Designed primarily for debugging.

Show:

- Flow name and repository path
- Result and duration
- CI environment
- Device details
- Sequence of steps
- Failure message
- Last successful step
- Failed step
- Screenshot evidence
- Relevant debug log excerpts
- Full report download
- Link to source code
- Links to previous executions of the same flow

Use a navigable execution timeline, not a single enormous terminal log.

### Screen E — Test History

Filterable records across historical pipelines.

Filters:

- Project
- Branch
- Commit
- Time range
- Suite
- Device model
- Android/iOS version
- Status
- Tags
- Flow
- CI provider

Present trends:

- Pass rate
- Failure rate
- Duration distribution
- Slowest flows
- Most frequently failing flows
- Retry rate
- Environment-specific failures
- Historical runner utilization

### Screen F — Comparisons

Compare two test runs.

Examples:

- Main branch vs feature branch
- Current regression suite vs previous successful baseline
- Android API 35 vs API 36
- One emulator type vs another

Highlight:

- Newly failing flows
- Recovered flows
- Missing/new flows
- Duration regressions
- Changed environment
- Changed test count

Do not treat differences in selected tests or device configurations as direct performance regressions without explaining the mismatch.

### Screen G — Administration

Internal configuration:

- Project onboarding
- CI integration settings
- Authorized publishers
- User roles
- Data retention
- Artifact policy
- Metric collection policy
- Agent compatibility
- Event ingestion health
- Audit history

---

## 9. Failure intelligence

The dashboard should distinguish an observed failure from an inferred root cause.

### Evidence

Capture the actual Maestro failure text, failing step, screenshots, relevant logs, process exit status and device context.

### Classification

Start with deterministic categories:

```text
assertion_failure
element_not_found
timeout
application_crash
device_disconnected
emulator_boot_failure
installation_failure
test_configuration_error
infrastructure_failure
process_terminated
unknown
```

Classification must record its supporting evidence and confidence.

### Historical analysis

The system can calculate:

- Recurring failures by flow and step
- Failures isolated to a device or OS version
- Failures correlated with memory pressure
- Slow flows relative to their baseline
- Pass-after-retry rates
- New failures introduced by a branch

Flakiness should be measured over comparable execution conditions.

### Future optional capability

AI-assisted failure explanation, using sanitized logs and screenshots with company approval.

This is not needed for the initial release.

The system should first provide excellent raw evidence and deterministic failure grouping.

---

## 10. Security and company requirements

Assume the tool handles internal software, CI metadata, debug logs and potentially sensitive user-interface screenshots.

### Authentication

Dashboard users authenticate through company SSO, preferably OIDC.

Support roles such as:

- Viewer
- Maintainer
- Administrator

Provide project-level authorization if multiple teams share the platform.

### CI publisher authentication

Use short-lived, narrowly scoped credentials.

Prefer CI workload identity/OIDC when IT can support it.

A scoped CI secret is a fallback, not the ideal long-term credential architecture.

Agent credentials must not grant access to historical reports, administrative APIs or other projects.

### Network

- HTTPS/WSS only
- Outbound connection from CI to Fleet
- No inbound runner access
- No exposed shell or remote-execution functionality
- Configurable network allowlists where practical
- Corporate DNS/certificates supported

### Sensitive data

- Redact known secret patterns
- Never upload environment dumps
- Never forward raw credential-bearing process arguments
- Limit console log collection
- Protect screenshots and recordings as potentially sensitive data
- Use authorized artifact access with expiring download links
- Validate file types, sizes and archive contents
- Sanitize rendered log text
- Prevent HTML reports from executing untrusted content in the application's origin

Untrusted pull-request code must not receive privileged publishing credentials without an approved isolation and credential strategy.

### Audit

Record administrative changes, credential rotations, artifact access when required, and retention-policy changes.

### Deployment security

IT controls secrets, TLS, SSO integration, backups, logging, monitoring and the production deployment process.

---

## 11. Hosting-independent deployment

Provide a Docker-based distribution.

Suggested services:

```text
fleet-web
fleet-api
postgres
object-storage
```

Object storage can be external S3-compatible infrastructure or an internally managed service.

Web and API may share one deployable image initially if that simplifies IT operations.

Support:

- Environment-based configuration
- Secret injection
- Database migrations
- Health/readiness endpoints
- Reverse proxies with WebSocket support
- Graceful shutdown
- Structured application logs
- Backup and restore
- Versioned upgrades

No Cloudflare, AWS, Azure, or Kubernetes-specific code should exist in the core business logic.

Provide a Docker Compose reference deployment. Kubernetes manifests can be supplied later if IT requires them.

Keep the first deployment operationally small. Do not add Kafka, a time-series database, or Redis without demonstrated need.

---

## 12. CI integration

A CI job requires:

1. A working Maestro installation
2. An available emulator/device
3. Fleet Agent installed or downloaded
4. Fleet API address
5. Publisher credentials
6. CI metadata

Example:

```bash
maestro-fleet run \
  --project mobile-app \
  --suite smoke \
  -- \
  maestro test \
    --format junit \
    --output build/fleet/report.xml \
    --test-output-dir build/fleet/artifacts \
    --debug-output build/fleet/artifacts \
    .maestro/
```

The CI provider supplies workflow ID, attempt, job ID, repository, branch, commit and PR metadata through a provider adapter.

Supported initial adapters:

- Generic CI environment
- GitHub Actions

Future:

- GitLab CI
- Jenkins
- Buildkite
- Other providers

The adapter must not require the Fleet backend to contact the CI provider.

**Important:** A job's successful finalization does not necessarily imply the entire CI pipeline succeeded. A future provider integration or pipeline-level completion event can establish the authoritative workflow result.

---

## 13. Failure and interruption behavior

### WebSocket disconnect

Keep Maestro executing. Buffer essential events and reconnect.

### Backend unavailable

Preserve the original CI behavior. Retain local diagnostic evidence and report a monitoring failure separately.

### Emulator crash

Capture the device failure and associate it with the affected executions.

### Maestro failure

Preserve its exit code. Publish failure evidence and finalize available results.

### CI cancellation

Forward termination to the Maestro process, attempt bounded cleanup and mark the execution cancelled or interrupted.

### Runner terminated without cleanup

Backend detects heartbeat expiration and marks state stale, then interrupted/unknown according to configured policy.

### Report absent

Keep live-observed results with an explicit evidence status. Never fabricate a successful final report.

### Partial report

Reconcile supported results without forcing every missing flow to failed.

### Backend restart

Restore active execution state from persisted records and resume accepted agent connections.

### Duplicate or out-of-order messages

Reject stale transitions where necessary and apply idempotent event processing.

---

## 14. Delivery phases

### Phase 0 — Integration proof

Before developing the full platform, prove the telemetry model against a real Maestro suite.

Deliver:

- Captured console samples
- Final JUnit examples
- Command artifact examples
- Multi-device/shard samples
- Failure and interruption samples
- Confirmed device-correlation rules

Test at least one successful flow, one failure, one multi-flow suite and one sharded execution.

**Gate:** establish exactly which progress states are observable, inferred or unavailable.

### Phase 1 — Agent

Build the CI wrapper, telemetry collector and WebSocket publisher.

Deliver:

- Process supervision
- Metrics collection
- Device discovery
- Live event extraction
- Local buffer/reconnect
- JUnit reconciliation
- CLI diagnostics
- Unit/integration tests

**Gate:** a real CI run streams safely without altering Maestro's result.

### Phase 2 — Server

Build the backend and persistence layer.

Deliver:

- Agent authentication
- Versioned event protocol
- Durable ingestion and acknowledgments
- Relational models
- Live state projection
- Browser broadcast
- Run-history API

**Gate:** concurrent agents can publish without corrupting run state.

### Phase 3 — Dashboard

Build the primary web interface.

Deliver:

- Fleet overview
- Live pipeline page
- Worker/device health
- Flow investigation
- Responsive charts
- Reconnection and stale-state handling

**Gate:** developers can monitor a complete test run without opening CI logs.

### Phase 4 — Evidence and history

Deliver:

- JUnit normalization
- Artifact uploads/downloads
- Screenshot/log viewer
- Failure classification
- Test history
- Duration trends
- Run comparisons
- Retention enforcement

**Gate:** a developer can diagnose a failed run after the CI machine no longer exists.

### Phase 5 — Production hardening

Deliver:

- SSO/RBAC
- Audit controls
- Security review
- Load tests
- Backup/restore verification
- Host portability tests
- Documentation
- Deployment package
- IT handover

**Gate:** IT can deploy and maintain the platform independently.

### Phase 6 — Optional capabilities

Only after the core platform is stable:

- Flaky-test detection and ranking
- Automatic regression comparisons
- Notifications
- AI failure summaries
- iOS simulators
- Device-farm integrations
- Cross-project analytics
- Custom quality-gate recommendations

---

## 15. Testing strategy

### Unit testing

Protocol validation, parsing, state machines, report reconciliation, metrics normalization, artifact filtering and failure classification.

### Integration testing

Use real Maestro-generated fixtures, then execute suites against actual emulators.

### Resilience testing

- Drop WebSocket during a flow
- Restart backend mid-run
- Terminate CI worker abruptly
- Duplicate events
- Delay and reorder events
- Fill agent buffer
- Produce enormous logs
- Generate invalid/partial reports
- Run concurrent jobs with overlapping names
- Retry failed flows
- Use multiple shards/devices
- Simulate clock skew
- Produce unavailable thermal and device metrics

### Compatibility testing

Maintain fixtures from supported Maestro versions.

Changes in CLI output must not silently create false pass/fail results.

### Performance testing

Measure agent overhead and server performance under realistic concurrent runner loads.

Compare baseline Maestro execution duration with and without Fleet enabled.

### Security testing

Validate authentication boundaries, artifact authorization, secret redaction, malicious log rendering, upload limits and untrusted PR behavior.

---

## 16. Acceptance criteria

A production-ready first release must demonstrate:

1. A CI job can publish live telemetry without SSH.
2. The browser updates during execution, without manual refresh.
3. Multiple CI jobs and emulators remain correctly separated.
4. Host and device metrics have accurate labels and units.
5. Missing thermal sensors show unavailable, never fake values.
6. Flow progress is accurate or explicitly marked uncertain.
7. Final results reconcile with Maestro's JUnit output.
8. Failures link to actionable debugging evidence.
9. Test history remains accessible after the runner disappears.
10. Network interruptions do not cause tests to fail.
11. Fleet preserves the original test exit code and CI quality gate.
12. Private test data is protected by company authentication.
13. IT can deploy it without depending on one hosting provider.
14. The web experience remains usable with multiple simultaneous active runs.
15. Failed and interrupted jobs cannot appear falsely successful.

## 17. Non-goals for v1

Do not build:

- A new test runner
- CI orchestration or job scheduling
- Remote shell access
- Emulator start/stop controls from the browser
- Remote desktop or live device streaming
- Automatic test rewriting
- Automatic failure retries
- General-purpose application performance monitoring
- A complete replacement for CI-provider interfaces
- AI-based autonomous debugging

Keep the product focused on reliable Maestro test observability.

## 18. Final architectural principle

**Every displayed result needs a provenance.**

A user must be able to distinguish:

- Directly observed information
- Provisional live information
- Verified final test results
- Derived metrics
- Inferred failure explanations
- Missing or stale information

Accuracy, operational simplicity and debugging value matter more than the number of metrics displayed.

The first release succeeds when the engineering team can run Maestro in CI, open Fleet, watch the suite execute live, and investigate failures afterward without needing direct access to the runner.
