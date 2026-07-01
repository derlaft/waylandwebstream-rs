# Builder image for waylandwebstream Debian packaging.
#
# Mirrors the dev box: Debian 13 (trixie), FFmpeg 7.1. The binary's runtime ABI
# therefore matches what end users install on trixie. GitHub's ubuntu-latest
# ships a different FFmpeg, which is exactly why CI must build inside THIS image
# rather than directly on the runner.
#
# Reproducible builds are deferred (see plan M5): this pins the suite (trixie)
# but not yet an image digest or exact apt package versions.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

# Build toolchain + native -dev deps (package names verified against trixie).
# clang/libclang-dev are required because ffmpeg-sys-next generates its bindings
# with bindgen at build time. Node/npm build the embedded Svelte/Vite bundle
# (build.rs -> web/dist -> rust-embed).
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

# Rust via rustup (Debian's apt rustc lags the dev toolchain). Pinning the exact
# channel is part of the deferred reproducibility work (plan M5); for now we
# track stable.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustc --version && cargo --version

# nfpm (single Go binary) builds the .deb (and, later, .rpm) from one nfpm.yaml.
ARG NFPM_VERSION=2.47.0
RUN curl -fsSL "https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz" \
      | tar -xz -C /usr/local/bin nfpm \
    && nfpm --version

WORKDIR /work
