# syntax=docker/dockerfile:1
#
# Multi-stage build for waylandwebstream: the entire build/test/package pipeline
# lives here, so `docker build` reproduces CI exactly and can be run locally.
#
#   docker build --target test    .                         # lint + tiered tests
#   docker build --target artifact -o out .                 # writes out/*.deb
#
# Base is Debian 13 (trixie, FFmpeg 7.1) to match the runtime ABI users install.
# The .deb targets amd64 (the published repo). On a non-amd64 host, build with
# `--platform linux/amd64`.

ARG DEBIAN_TAG=trixie-slim

########################## base: toolchain ##################################
FROM debian:${DEBIAN_TAG} AS base
ENV DEBIAN_FRONTEND=noninteractive
# Build toolchain + native -dev deps (names verified on trixie). clang/libclang
# are needed because ffmpeg-sys-next runs bindgen at build time; node/npm build
# the embedded Svelte/Vite bundle.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git xz-utils just \
      build-essential pkg-config clang libclang-dev \
      nodejs npm \
      libavcodec-dev libavdevice-dev libavfilter-dev libavformat-dev \
      libavutil-dev libswscale-dev \
      libwayland-dev libpixman-1-dev \
      libgbm-dev libdrm-dev libegl-dev libgl-dev libgles-dev \
      libpipewire-0.3-dev libopus-dev libxkbcommon-dev \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustup component add rustfmt clippy \
    && rustc --version
ARG NFPM_VERSION=2.47.0
RUN curl -fsSL "https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz" \
      | tar -xz -C /usr/local/bin nfpm && nfpm --version
WORKDIR /src

########################## web: embedded bundle #############################
FROM base AS web
COPY web/ web/
# npm ci is reproducible against the committed lockfile; produces web/dist and
# leaves node_modules for the test stage's typecheck/vitest.
RUN cd web && npm ci && npm run build

########################## build: release binary ###########################
FROM base AS build
COPY . .
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --locked -p waylandwebstream

########################## test: lint + tiered tests #######################
# A separate branch from build; `docker build --target test` fails if any
# blocking test fails. Test-only tooling (weston) is installed here so the
# build/package stages stay lean.
FROM base AS test
RUN apt-get update && apt-get install -y --no-install-recommends weston \
    && rm -rf /var/lib/apt/lists/*
COPY . .
COPY --from=web /src/web /src/web
ENV XDG_RUNTIME_DIR=/tmp/xdg
RUN mkdir -p /tmp/xdg && chmod 700 /tmp/xdg

# Formatting is blocking for the shipped crate (rustfmt-clean); the diagnostic
# crates are not gated.
RUN cargo fmt -p waylandwebstream -- --check

# clippy is strict (blocking) for the shipped crate; the diagnostic crates
# (native-client, wayland-test-client) are linted report-only for now.
RUN cargo clippy --locked -p waylandwebstream --all-targets -- -D warnings
RUN cargo clippy --locked --workspace --all-targets -- -D warnings \
    || echo "WARNING: clippy reported warnings in the diagnostic crates (non-blocking)"

# Web typecheck + unit tests (blocking).
RUN cd web && npm run check && npm test

# Rust test tiers (blocking).
RUN cargo test --locked --lib --bins
RUN cargo test --locked --test adaptive_bitrate_test --test latency_websocket_test
RUN cargo test --locked --test render_pixels_test

########################## package: build the .deb #########################
FROM build AS package
ARG WWS_VERSION=0.0.0
ENV WWS_VERSION=${WWS_VERSION}
RUN mkdir -p dist \
    && nfpm package --packager deb --config nfpm.yaml \
         --target "dist/waylandwebstream_${WWS_VERSION}_amd64.deb"

########################## artifact: export-only ###########################
# `docker build --target artifact -o <dir>` writes just the .deb(s) to <dir>.
FROM scratch AS artifact
COPY --from=package /src/dist/ /
