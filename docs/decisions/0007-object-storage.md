# ADR-0007: Object storage

**Status:** Accepted (shipped in M3 and M5, 2026-11-06) · **Date:** 2026-11-03 · **Research:** [06 §7](../../research/06-ci-security-deployment.md), [03 §7](../../research/03-rust-server.md)

## Context
MinIO community edition: console removed (2025), binary and Docker distribution stopped (Oct 2025), repository archived (2026-04-25).

## Decision
- **Library:** `object_store` 0.14. It supports a custom endpoint with path-style S3, presigned PUT/GET and multipart parts, and has a `LocalFileSystem` backend.
- **Reference Compose default:** **Garage** (Rust, AGPLv3). **SeaweedFS** (Apache-2.0) if AGPL is unacceptable. Built-in **filesystem backend** for small installs and tests.
- **Uploads are relayed through Fleet by default.** Presigned direct upload is optional; it is built for S3 (see the implementation notes below).
- **Content-addressed:** blobs keyed by sha256 *per project*.
- **Retention class promoted at finalization**, for example a success screenshot becomes failure evidence when its flow fails.
- **User HTML** is served only from a separate usercontent origin with `CSP: sandbox`, or as a download.

## Consequences
IT must confirm the AGPL position, and provide a second hostname and certificate for user content.

## Implementation notes (M5-L)
- `object_store` is built with `aws-base`, `reqwest` and `ring`, not `aws`: that feature links `aws-lc-rs` and a second TLS backend, while Fleet runs rustls on `ring` everywhere. Only the generic S3 API is used (I8).
- One contract test suite (`crates/fleet-blob/tests/it/contract.rs`) runs against the filesystem backend and against Garage in Docker (v2.5.0, pinned by digest, the same `garage.toml` and flags as the reference Compose file).
- Verification (size, SHA-256, magic bytes, archive safety) lives in `fleet-blob::verify`, as pure functions over `Read + Seek`.
- **Direct upload is built (issue #43).** `BlobStore::presign_put` signs a `PUT` for a per-artifact staging object (`uploads/<artifact>`), with `x-amz-checksum-sha256` bound to the declared digest. `object_store` refuses to sign `content-length`, so the checksum is the signed size and content guard: S3 refuses any body that does not hash to it, which also fixes its length. The filesystem backend keeps the default (no presign), so it relays. Garage v2.5.0 enforces the signed checksum and ignores `If-None-Match`, which is why one-shot is enforced at `complete` (the staged object is read once) rather than by a conditional write. The server stores nothing from a staged object that verification refuses, and the blob is written only from the server's own spool copy. Staged objects are swept with the spool after twice the grant lifetime.
