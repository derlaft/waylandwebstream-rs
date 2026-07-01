# waylandwebstream build / package helpers.
#
# The build/test/package pipeline lives in the multi-stage Dockerfile, so these
# recipes are thin wrappers over `docker build`. The .deb targets amd64; on a
# non-amd64 host the default `--platform linux/amd64` emulates it.

# Package version: default from Cargo.toml, override with `WWS_VERSION=... just ...`.
export WWS_VERSION := env_var_or_default("WWS_VERSION", `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`)
platform := env_var_or_default("PLATFORM", "linux/amd64")

# List available recipes.
default:
    @just --list

# Run the full test tier (lint + tiered tests) in the Dockerfile `test` stage.
test:
    docker build --platform {{platform}} --target test .

# Build the .deb into dist/ via the Dockerfile `artifact` stage.
package:
    docker build --platform {{platform}} --target artifact \
      --build-arg WWS_VERSION="$WWS_VERSION" -o dist .
    @ls -1 dist/*.deb

# Native release build (no Docker) -- requires the toolchain on the host.
build:
    cd web && npm ci && npm run build
    cargo build --release --locked -p waylandwebstream

# Regenerate nfpm's `depends:` from the built binary's direct DT_NEEDED libs.
# Run after `just build`; paste the printed `shlibs:Depends=` into nfpm.yaml.
deps:
    #!/usr/bin/env bash
    set -euo pipefail
    bin=target/release/waylandwebstream
    [ -x "$bin" ] || { echo "build first: just build"; exit 1; }
    tmp=$(mktemp -d); mkdir -p "$tmp/debian"
    printf 'Source: waylandwebstream\nPackage: waylandwebstream\nArchitecture: amd64\n' > "$tmp/debian/control"
    cp "$bin" "$tmp/wws"
    ( cd "$tmp" && dpkg-shlibdeps -O ./wws 2>/dev/null )
    rm -rf "$tmp"

clean:
    rm -rf dist
