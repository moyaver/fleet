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
# Trunk's tools for the dashboard build, installed here rather than by trunk. Trunk would download
# them after COPY . ., so every source change repeated the downloads: wasm-opt (~108 MB) and
# tailwindcss (~111 MB) are GitHub archives that stall on slow networks, and trunk restarts a download
# from zero. Trunk uses a tool on PATH when no version is configured (wasm-opt, tailwindcss), and
# wasm-bindgen's version here is the one Cargo.lock pins. Every download is pinned by checksum, and
# the loop resumes a stalled one.
RUN set -eux; \
    case "$(uname -m)" in \
      x86_64) \
        binaryen=x86_64-linux; binaryen_sha=b5bf1f0eaf17c63ee588ff7a5954dc8f6ce2c26989051c66f24dfe9ece3e46db; \
        tailwind=tailwindcss-linux-x64; tailwind_sha=dc61b3ac6b8c9ca874c0cc4c57b2409791a64c5540404ca5f5367360babc313a; \
        wbg=x86_64-unknown-linux-musl; wbg_sha=9687f3bb551b9f66f37ed32a40d7913a00f7962db653054e56857bc306c3227f ;; \
      aarch64) \
        binaryen=aarch64-linux; binaryen_sha=ba991f677edd9a21d2bc96c0144bc8ac5b112d4d98a3eb266e075e22e557df2a; \
        tailwind=tailwindcss-linux-arm64; tailwind_sha=55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195; \
        wbg=aarch64-unknown-linux-musl; wbg_sha=99ffc93ce80081b91632e2b539b782821a8f3439ab0fddc645287462b2f55d3d ;; \
      *) echo "no pinned trunk tools for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    fetch() { \
      for _ in $(seq 20); do \
        curl -fL --connect-timeout 30 --speed-limit 1024 --speed-time 30 -C - -o "$1" "$2" || true; \
        if echo "$3  $1" | sha256sum -c --status -; then return 0; fi; \
        sleep 5; \
      done; \
      return 1; \
    }; \
    cd /tmp; \
    fetch binaryen.tar.gz "https://github.com/WebAssembly/binaryen/releases/download/version_131/binaryen-version_131-${binaryen}.tar.gz" "$binaryen_sha"; \
    tar -xzf binaryen.tar.gz -C /usr/local/bin --strip-components=2 binaryen-version_131/bin/wasm-opt; \
    fetch tailwindcss "https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.3/${tailwind}" "$tailwind_sha"; \
    install -m 0755 tailwindcss /usr/local/bin/tailwindcss; \
    fetch wasm-bindgen.tar.gz "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.130/wasm-bindgen-0.2.130-${wbg}.tar.gz" "$wbg_sha"; \
    tar -xzf wasm-bindgen.tar.gz -C /usr/local/bin --strip-components=1 "wasm-bindgen-0.2.130-${wbg}/wasm-bindgen"; \
    rm -f binaryen.tar.gz tailwindcss wasm-bindgen.tar.gz; \
    wasm-opt --version; \
    wasm-bindgen --version; \
    tailwindcss --help > /dev/null
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
RUN cargo build --release --locked -p fleet-server --bin fleet \
 && mkdir -p /out/spool /out/blobs

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /src/target/release/fleet /usr/local/bin/fleet
COPY LICENSE /usr/share/doc/fleet/LICENSE
# The upload spool (and the blob directory of a `local_fs` install), writable by `nonroot`; mount
# volumes over them.
COPY --from=builder --chown=nonroot:nonroot /out/spool /var/lib/fleet/spool
COPY --from=builder --chown=nonroot:nonroot /out/blobs /var/lib/fleet/blobs
ENV FLEET_LISTEN=0.0.0.0:8080
EXPOSE 8080
USER nonroot
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=6 \
  CMD ["/usr/local/bin/fleet", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/fleet"]
CMD ["serve"]
