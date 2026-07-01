# waylandwebstream packaging / build helpers.
#
# Native targets (build, web, package, deps) assume a Debian trixie-like
# environment with the toolchain present (the dev box, or inside the builder
# image). The `docker-*` wrappers run them inside Dockerfile.builder so they
# work anywhere Docker does (e.g. GitHub's ubuntu runners).

# Package version: default from Cargo.toml, override with `WWS_VERSION=... just ...`.
export WWS_VERSION := env_var_or_default("WWS_VERSION", `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`)

builder_image := "waylandwebstream-builder"

# List available recipes.
default:
    @just --list

# Build the embedded web bundle (Svelte/Vite).
web:
    cd web && npm ci && npm run build

# Release build of the server binary (depends on the web bundle).
build: web
    cargo build --release --locked -p waylandwebstream

# Build the .deb into dist/ (runs `build` first).
package: build
    mkdir -p dist
    nfpm package --packager deb --config nfpm.yaml --target "dist/waylandwebstream_${WWS_VERSION}_amd64.deb"
    @ls -1 dist/*.deb

# Regenerate nfpm's `depends:` from the built binary's direct DT_NEEDED libs.
# Paste the printed `shlibs:Depends=` value into nfpm.yaml.
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

# --- Docker wrappers (build inside the pinned trixie builder) ---------------

# Build the builder image.
builder-image:
    docker build -f Dockerfile.builder -t {{builder_image}} .

# Run any recipe inside the builder image, e.g. `just in-docker package`.
in-docker +recipe: builder-image
    docker run --rm -v "$PWD":/work -w /work \
        -e WWS_VERSION="$WWS_VERSION" \
        {{builder_image}} just {{recipe}}

# Convenience: build the .deb inside the builder image.
docker-package: (in-docker "package")

clean:
    rm -rf dist target/release/waylandwebstream
