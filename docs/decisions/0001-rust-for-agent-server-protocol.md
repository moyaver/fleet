# ADR-0001: Rust for agent, server and shared protocol

**Status:** Accepted (project direction) · **Date:** 2026-11-03 · **Supersedes:** spec v1.0 §4 "Runtime" (Python) and §6 "Backend stack" (TypeScript/Fastify)

## Context
The spec proposed a Python agent and a Node/TypeScript server. The project direction is Rust.

## Decision
- **Agent:** one static Rust binary per target: Linux musl x86_64/aarch64, macOS aarch64.
- **Server:** one Rust binary (axum 0.8, sqlx 0.9, object_store 0.14, openidconnect 4, jsonwebtoken 11, utoipa 6).
- **`fleet-protocol` crate:** dependency-light and wasm-safe. It holds every wire type and REST DTO and generates JSON Schema (schemars) and TypeScript types (ts-rs).
- **Cargo workspace layout:** `fleet-protocol`, `fleet-core` (pure domain: state machines, reconciliation, provenance, policy), `fleet-store`, `fleet-blob`, `fleet-server`, `fleet-agent`, `fleet-loadgen`. See research 03 §15.

## Consequences
- Agent: 6–7.6 MB binary, ~0.02% of one core, ~10 MB RSS (measured, research 02). No interpreter needed on runners.
- Server: an estimated 15–30% more effort than TypeScript, a ~35–40 MB distroless image, and slower cold builds (~11 min release). Small ecosystem gaps (session store, TLS features) need workarounds (research 03 §0).
- The dashboard is Rust too (Leptos CSR): [ADR-0004](0004-frontend.md). The product contains no Node.js and no hand-written JS.
