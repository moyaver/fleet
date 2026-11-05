# Security model

Fleet authenticates browser readers through company OIDC and scopes publishers to one project. The role policy grants viewers access to runs and evidence. Publishers can ingest and upload evidence for their project. They cannot read private runs or artifacts.

## Secret export policy

I6 covers a measurable set of secret sources and representations. The agent collects values from `-e` and `--env`, flow-data `MAESTRO_*` variables, and environment names containing token, secret, password, passwd, pass, key, credential, auth, private, session or cookie. CLI configuration variables are excluded. Values shorter than four characters are excluded from global replacement because they occur throughout unrelated output.

Known values are replaced in plain text, JSON-escaped text, percent encoding with upper- or lowercase hex digits, and standard or URL-safe base64 with or without padding. Token patterns cover GitHub tokens, Fleet API keys, AWS access-key IDs, Slack tokens, bearer tokens, JWT shapes and PEM private keys. The captured Phase 0 planted secrets and adversarial encoding tests exercise these rules. The same redactor handles event text and exported text artifacts.

Before exporting `commands.json`, Fleet withholds resolved `evaluatedCommand` payloads and variable definitions while retaining command types, status, sequence, depth and duration. This protects values created inside the flow that are absent from the runner environment.

Text artifacts must be valid UTF-8, contain no NUL bytes and fit within the 16 MiB per-file redaction limit. Otherwise the agent records `not_redactable` and sends no upload request. Redaction runs before hashing and transmission, so the digest identifies the exported bytes.

A secret assembled across distinct events or transformed with an unrecognized encoding may fall outside these rules. Flow configuration and test data should use synthetic accounts. A secret whose value is known only to the application can appear in logs and pixels. Stop evidence export with `FLEET_UPLOAD_ARTIFACTS=off` when those artifacts cannot be shared with project readers. Maestro's terminal output remains visible to the CI log consumer under the supervisor contract.

## Uploaded files

Artifacts are untrusted input. Uploads are allowlisted by class, limited by size and run, and verified by size, SHA-256, content type and archive safety before storage. Storage keys include the project and digest. Requests authorize the project before exposing an artifact or issuing its signed URL.

Screenshots are sensitive binary media. The agent uploads failure screenshots, and their pixels receive no automatic redaction. Project artifact permissions, retention and short-lived links control access. An authorized reader can retain a downloaded copy. The separate usercontent origin serves evidence with sandbox headers and no application cookies. Configure a distinct registrable domain in production. [Operator guide](operator-guide.md#usercontent-domain)

The export threats include secrets embedded in Maestro commands, unknown runtime values in logs, personal information in screenshots, malicious HTML, archive bombs, cross-project references and replayed upload grants. The controls above are exercised by redactor, uploader, artifact verification and project access tests. Retention removes Fleet's stored references and unreferenced blobs after the configured grace period. Backups retain data until their own retention expires.
