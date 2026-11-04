# syntax=docker/dockerfile:1
# SPDX-License-Identifier: AGPL-3.0-only
# The Fleet server image (PLAN §5.3, research 03 §13): cargo-chef caches the dependency build,
# Trunk builds the dashboard that the binary embeds, and the runtime is distroless (glibc, CA
# certificates, no shell). Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t fleet .

ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-trixie AS chef
# The image's toolchain, not rust-toolchain.toml's (which also lists cross targets).
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} CARGO_TERM_COLOR=always
RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked cargo-chef trunk
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p fleet-server
COPY . .
RUN cd crates/fleet-web && trunk build --release --locked
# No database at build time: queries are checked against the committed .sqlx data.
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked -p fleet-server --bin fleet

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /src/target/release/fleet /usr/local/bin/fleet
COPY LICENSE /usr/share/doc/fleet/LICENSE
ENV FLEET_LISTEN=0.0.0.0:8080
EXPOSE 8080
USER nonroot
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=6 \
  CMD ["/usr/local/bin/fleet", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/fleet"]
CMD ["serve"]
