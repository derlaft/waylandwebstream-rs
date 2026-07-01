# Testing

The test suite is split into tiers by what each test needs to run. CI runs the
two automated tiers; the heavier browser- and GPU-driven tiers are run manually.

## Tier 1 — fast, no external tools (CI gate)

Pure Rust, no display, no external binaries. Run anywhere:

```sh
cargo test --locked --lib --bins                 # unit tests (server + native-client lib)
cargo test --locked --test adaptive_bitrate_test --test latency_websocket_test
cd web && npm ci && npm run check && npm test    # svelte-check + tsc + vitest
```

## Tier 2 — software graphical (CI gate)

`render_pixels_test` drives the compositor's `render()` in-process and asserts a
non-black composited frame. It needs only the `weston-simple-shm` client (from
the `weston` package) connecting to our headless compositor — **no GPU, no
`/dev/dri`, no browser**. It skips gracefully (`which("weston-simple-shm")`) when
the binary is absent.

```sh
# Debian/Ubuntu: provides weston-simple-shm
sudo apt-get install -y weston
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
cargo test --locked --test render_pixels_test
```

## Tier 3 — browser-driven (manual)

These spin up the full pipeline and validate it through a real browser via
Puppeteer/Chromium, and force a `--release` build internally. They are **not run
in CI** (heavy, and historically flaky). They need Node + a Chromium that
Puppeteer can drive.

Tests: `integration_test`, `cursor_test`, `keyboard_latency_test`,
`mouse_latency_test`, `touch_latency_test`, `keyboard_focus_loss_test`.

```sh
cd tests && npm install && cd ..     # Puppeteer + helpers
./run_integration_test.sh            # full browser-driven pipeline
# or a single one:
cargo test --release --test cursor_test -- --nocapture
```

`run_integration_test.sh` validates compositor startup, Wayland client
rendering, WebSocket/WebCodecs streaming, and screenshot output end to end.

## Tier 4 — nested compositor / GPU (manual, hardware-dependent)

`cage_rendering_test` renders through a nested `cage` (wlroots) compositor and
wants a real DRM node. cage's wlroots Wayland backend fails its output test
without `/dev/dri`, so this tier needs GPU-capable hardware; it skips gracefully
when `cage`/`weston-simple-shm` are missing.

The VA-API hardware H.264 encode path (`--encoder vaapi`) likewise needs a
`/dev/dri` render node and is exercised manually on such hardware; the software
x264 path is the default and is what the other tiers cover.

```sh
sudo apt-get install -y cage weston
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
cargo test --locked --test cage_rendering_test
```

## What CI runs

The `test` workflow (`.github/workflows/test.yml`) drives the multi-stage
`Dockerfile` (Debian trixie, FFmpeg 7.1). Its `test` stage runs `cargo fmt
--check` and `cargo clippy -D warnings` (report-only for now), the web `check` +
`vitest`, and the Tier 1 + Tier 2 Rust tests (with `weston` for Tier 2); its
`artifact` stage smoke-builds the `.deb`. Run the same thing locally with
`just test` and `just package` (or `docker build --target test .`). Tiers 3 and
4 are not run in CI.
