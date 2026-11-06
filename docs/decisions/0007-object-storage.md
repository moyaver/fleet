# ADR-0007: Object storage

**Status:** Accepted (shipped in M3 and M5, 2026-11-06) · **Date:** 2026-11-03 · **Research:** [06 §7](../../research/06-ci-security-deployment.md), [03 §7](../../research/03-rust-server.md)

## Context
MinIO community edition: console removed (2025), binary and Docker distribution stopped (Oct 2025), repository archived (2026-04-25).

## Decision
- **Library:** `object_store` 0.14. It supports a custom endpoint with path-style S3, presigned PUT/GET and multipart parts, and has a `LocalFileSystem` backend.
- **Reference Compose default:** **Garage** (Rust, AGPLv3). **SeaweedFS** (Apache-2.0) if AGPL is unacceptable. Built-in **filesystem backend** for small installs and tests.
- **Uploads are relayed through Fleet by default.** Presigned direct upload is optional.
- **Content-addressed:** blobs keyed by sha256 *per project*.
- **Retention class promoted at finalization**, for example a success screenshot becomes failure evidence when its flow fails.
- **User HTML** is served only from a separate usercontent origin with `CSP: sandbox`, or as a download.

## Consequences
IT must confirm the AGPL position, and provide a second hostname and certificate for user content.

## Implementation notes (M5-L)
- `object_store` is built with `aws-base`, `reqwest` and `ring`, not `aws`: that feature links `aws-lc-rs` and a second TLS backend, while Fleet runs rustls on `ring` everywhere. Only the generic S3 API is used (I8).
- One contract test suite (`crates/fleet-blob/tests/it/contract.rs`) runs against the filesystem backend and against Garage in Docker (v2.5.0, pinned by digest, the same `garage.toml` and flags as the reference Compose file).
- Verification (size, SHA-256, magic bytes, archive safety) lives in `fleet-blob::verify`, as pure functions over `Read + Seek`.
