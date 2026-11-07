# Security model

Fleet authenticates browser readers through company OIDC and scopes publishers to one project. The role policy grants viewers access to runs and evidence. Publishers can ingest and upload evidence for their project. They cannot read private runs or artifacts.

## Secret export policy

I6 covers a measurable set of secret sources and representations. The agent collects values from `-e` and `--env`, flow-data `MAESTRO_*` variables, and environment names containing token, secret, password, passwd, pass, key, credential, auth, private, session or cookie. CLI configuration variables are excluded. Values shorter than four characters are excluded from global replacement because they occur throughout unrelated output.

Known values are replaced in plain text, JSON-escaped text, percent encoding with upper- or lowercase hex digits, and standard or URL-safe base64 with or without padding. Token patterns cover GitHub tokens, Fleet API keys, AWS access-key IDs, Slack tokens, bearer tokens, JWT shapes and PEM private keys. The captured Phase 0 planted secrets and adversarial encoding tests exercise these rules. The same redactor handles event text and exported text artifacts.

UI hierarchies are exported with the text they show hidden: the string value of every `text`, `accessibilityText`, `hintText` and `error` attribute becomes `[REDACTED]`, and the tree, its classes, bounds and resource IDs stay. The same redactor then runs over the result, so a known value is replaced wherever it appears. A hierarchy that is not JSON of the expected shape, or that cannot be redacted reliably, is not uploaded. `text` and `accessibilityText` appear in the captured fixtures; `hintText` and `error` are covered by a synthetic test only. Any other text attribute that Maestro adds later would pass through unredacted.

Device logcat excerpts of a failed flow are exported as a window: from 10 seconds before the failing step starts to 2 seconds after it ends, capped at 512 KiB. Each kept line goes through the same redactor, and the cap counts redacted bytes. When the cap cuts the window, the newest lines are kept. The log's clock is aligned with the failure using Maestro's own last line, and an excerpt whose clock cannot be aligned is not uploaded. Text that is not UTF-8 or contains NUL bytes is not sent.

Before exporting `commands.json`, Fleet withholds resolved `evaluatedCommand` payloads and variable definitions while retaining command types, status, sequence, depth and duration. This protects values created inside the flow that are absent from the runner environment.

Text artifacts must be valid UTF-8, contain no NUL bytes and fit within the 16 MiB per-file redaction limit. Otherwise the agent records `not_redactable` and sends no upload request. Redaction runs before hashing and transmission, so the digest identifies the exported bytes.

A secret assembled across distinct events or transformed with an unrecognized encoding may fall outside these rules. Flow configuration and test data should use synthetic accounts. A secret whose value is known only to the application can appear in logs and pixels. Stop evidence export with `FLEET_UPLOAD_ARTIFACTS=off` when those artifacts cannot be shared with project readers. Maestro's terminal output remains visible to the CI log consumer under the supervisor contract.

## Request limits

Sign-in, agent and viewer requests are limited per client address or per credential, and a refused request gets `429` with `Retry-After` ([operator guide](operator-guide.md#client-addresses-and-limits)). A viewer credential is charged only after it is accepted: a made-up token gets `401` and no limiter state, so random credentials cannot fill the limiter. Anonymous reads (`api.anonymous_read`, development only) are not limited, so enable them only for synthetic data.

## Uploaded files

Artifacts are untrusted input. Uploads are allowlisted by class, limited by size and run, and verified by size, SHA-256, content type and archive safety before storage. Storage keys include the project and digest. Requests authorize the project before exposing an artifact or issuing its signed URL.

Screenshots are sensitive binary media. The agent uploads failure screenshots, and their pixels receive no automatic redaction. Maestro's own `takeScreenshot` images and `startRecording` videos are not uploaded at all, because they cannot be redacted; adding them needs its own policy ([#133](https://github.com/moyaver/fleet/issues/133)). Project artifact permissions, retention and short-lived links control access. An authorized reader can retain a downloaded copy. The separate usercontent origin serves evidence with sandbox headers and no application cookies. Configure a distinct registrable domain in production. [Operator guide](operator-guide.md#usercontent-domain)

The export threats include secrets embedded in Maestro commands, unknown runtime values in logs, personal information in screenshots, malicious HTML, archive bombs, cross-project references and replayed upload grants. The controls above are exercised by redactor, uploader, artifact verification and project access tests. Retention removes Fleet's stored references and unreferenced blobs after the configured grace period. Backups retain data until their own retention expires.
