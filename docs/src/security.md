# Security model

The threat model from research report 06 and secret-handling guidance. Never pass secrets to Maestro with `-e`.

> Written in milestone M6. See [PLAN §14](https://github.com/moyaver/fleet/blob/main/docs/PLAN.md#14-documentation-mdbook-docssrc).

## Uploaded files

Artifacts are untrusted input (threats T9, T11, T12 and T15 in research 06). Uploads are allowlisted by class, size-limited per class and per run, verified by content (size, SHA-256, magic bytes, archive safety) before anything is stored, and keyed by project and digest so a project can never read another's blobs or learn that they exist. Text leaves the runner redacted, or not at all (invariant I6). Files are served only from the usercontent origin, sandboxed and cookieless, by signed links of at most a few minutes (invariant I7); see the [operator guide](operator-guide.md#usercontent-domain).
