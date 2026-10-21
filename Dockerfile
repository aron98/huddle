# syntax=docker/dockerfile:1

# Multi-stage build producing a single-image deployment: the web client is
# compiled to wasm with trunk, the binary embeds the bundle (ADR 004), and the
# runtime stage is a slim Debian with just the huddle binary.

# --- web client: rust -> wasm via trunk ---------------------------------
FROM rust:1.96.0-slim-bookworm AS web

RUN rustup target add wasm32-unknown-unknown

# Trunk from the upstream release (mirrors the pinned version in CI).
ADD https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-x86_64-unknown-linux-musl.tar.gz /tmp/trunk.tar.gz
RUN tar -xzf /tmp/trunk.tar.gz -C /usr/local/bin && rm /tmp/trunk.tar.gz

WORKDIR /build
COPY . .
RUN cd crates/web && trunk build --release

# --- server binary: embeds the freshly built web bundle -----------------
FROM rust:1.96.0-slim-bookworm AS server

# aws-lc-sys (the rustls crypto backend) needs cmake + perl at build time;
# cmake's default Unix Makefiles generator needs make (gcc already ships in
# the rust slim image). Mirrors "apk add build-base cmake perl" in
# Dockerfile.release and the install-build-deps step of release.yml.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake make perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
COPY --from=web /build/crates/web/dist ./crates/server/assets
RUN cargo build --release --bin huddle

# --- runtime -------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=server /build/target/release/huddle /usr/local/bin/huddle

ENV HUDDLE_BIND=0.0.0.0:3000 \
    HUDDLE_DATA_DIR=/data

VOLUME /data
EXPOSE 3000

ENTRYPOINT ["huddle"]
