# WaylandWebStream

A single-binary service that runs a headless Wayland compositor and streams it
to a browser over a binary WebSocket, decoded client-side with WebCodecs, with
low-latency video and remote touch/input control.

## Overview

WaylandWebStream creates a full headless Wayland environment (via
[Smithay](https://github.com/Smithay/smithay)), encodes the compositor
framebuffer to H.264 in real time (via
[FFmpeg/rust-ffmpeg](https://github.com/zmwangx/rust-ffmpeg)), and delivers the
video stream to a browser client over a binary WebSocket (`/stream`), where the
browser's `VideoDecoder` (WebCodecs) decodes each frame straight into a
`<canvas>`. The user controls the remote desktop through touch/pointer events
sent back over a second WebSocket (`/ws`) and injected directly into the
compositor's input pipeline.

```
┌─────────────────────────────────────────────────────┐
│                   Server (single binary)            │
│                                                     │
│  ┌──────────────┐   framebuffer   ┌──────────────┐  │
│  │   Smithay    │ ──────────────> │   FFmpeg      │  │
│  │  Headless    │                 │  H.264 enc    │  │
│  │  Compositor  │                 └──────┬───────┘  │
│  └──────┬───────┘                        │          │
│         │ inject                  H.264 packets     │
│         │ input                          │          │
│  ┌──────┴───────┐                 ┌──────▼───────┐  │
│  │   Input      │ <─── /ws ────── │  /stream      │  │
│  │   Handler    │                 │  WebSocket    │  │
│  └──────────────┘                 └──────┬───────┘  │
│                                          │          │
│  ┌──────────────┐                        │          │
│  │ HTTP/WS      │ ◄──── /ws ──────────────┘          │
│  │ server (axum)│                                   │
│  └──────────────┘                                   │
└──────────────────────────────────────────────────────┘
          ▲                              │
          │           Network            │
          ▼                              ▼
┌─────────────────────────────────────────────────────┐
│                Browser Client                       │
│  ┌──────────┐  ┌───────────┐  ┌──────────────────┐  │
│  │ <canvas> │  │ WebCodecs │  │  Touch/Input     │  │
│  │ element  │  │  decoder  │  │  event capture   │  │
│  └──────────┘  └───────────┘  └──────────────────┘  │
└─────────────────────────────────────────────────────┘
```

## Key Features

- **Single binary** -- compositor, encoder, HTTP/WebSocket server, and web
  client all in one executable
- **Headless Wayland compositor** -- Smithay-based, no GPU or display required
- **Low-latency H.264 streaming** -- software encoding via FFmpeg (x264),
  tuned for real-time with `zerolatency` preset
- **Client-reported latency feedback** -- the browser reports encode/network/
  decode timing back over `/ws` so the server can be tuned against real
  glass-to-glass latency
- **Touch-first input** -- multi-touch, pointer, and physical-keyboard events
  relayed from the browser and injected directly into the compositor (no
  uinput kernel round-trip)
- **WebSocket + WebCodecs transport** -- one binary WebSocket per frame, no
  SDP/ICE negotiation; the browser's native `VideoDecoder` does the decoding
- **Built-in HTTP/WebSocket server** -- serves the web client, the binary
  video stream, and the input/control channel from a single built-in server;
  no separate signaling server or external infrastructure needed
- **Dynamic resolution** -- viewport size is negotiated per-client and can be
  changed mid-session; the compositor output, encoder, and stream adapt on
  the fly without reconnecting
- **Audio streaming** -- planned for a later phase (PipeWire capture)

## Architecture

| Component | Library | Role |
|---|---|---|
| Compositor | [smithay](https://github.com/Smithay/smithay) | Headless Wayland compositor with software rendering; dynamic output resizing |
| Video encoding | [ffmpeg-next](https://github.com/zmwangx/rust-ffmpeg) | H.264 encoding from raw framebuffer pixels |
| Streaming | built-in (axum WebSocket) | Binary H.264 frame delivery over `/stream`, decoded client-side with WebCodecs |
| Control channel | built-in (hyper/axum) | HTTP + WebSocket (`/ws`) for input, resize, and latency messages |
| Input | direct Smithay injection | Touch/keyboard/mouse events injected into SeatState; keyboard forwards physical key identity (`KeyboardEvent.code`), so the browser's OS keyboard layout should match the server's XKB layout for correct characters |
| Web client | [Svelte](https://svelte.dev) + [Vite](https://vite.dev), embedded via `rust-embed` | `<canvas>` + WebCodecs decode, touch/pointer capture, collapsible stats panel; compiled to a static bundle and baked into the binary |

## Requirements

### Build dependencies

- Rust 1.75+ (2024 edition)
- Node.js/npm -- `build.rs` runs `npm ci && npm run build` in `web/` to
  produce the Svelte client bundle that gets embedded into the binary
  (falls back to a stale `web/dist/` with a warning if `npm` is missing)
- FFmpeg development libraries (libavcodec, libavformat, libavutil, libswscale)
- Wayland development libraries (libwayland-server)
- pkg-config

### Runtime

- Linux (Wayland is Linux-only)
- FFmpeg shared libraries
- No GPU required (software rendering + software encoding)
- A reachable TCP port for the HTTP/WebSocket server (plain WebSocket over
  TCP -- no UDP or NAT traversal needed)

## Building

```sh
cargo build --release
```

This builds the Svelte web client (`web/`) via `build.rs` and embeds the
resulting `web/dist/` into the binary, so `cargo build` alone is enough.

### Web client dev loop

For live-reloading frontend work, run the Rust server and the Vite dev
server side by side -- Vite proxies `/ws` and `/stream` to the backend so
you get HMR against a real compositor:

```sh
cargo run                  # backend on :8080
cd web && npm install && npm run dev   # frontend dev server (Vite prints its own port)
```

Edits under `web/src/**` trigger `build.rs` to rebuild the embedded bundle
on the next `cargo build`/`cargo run`.

## Usage

```sh
# Start the server (defaults: 1280x720 @ 60fps, listening on 0.0.0.0:8080)
./waylandwebstream

# Common options (run with --help for the full list):
#   --initial-resolution 1280x720   default resolution for new clients
#   --max-resolution 3840x2160      upper bound for client-requested resize
#   --framerate 60                  target capture/encode framerate
#   --bitrate 2000000               starting bitrate (adaptive by default)
#   --crf 23                        constant-quality mode instead of a bitrate
#   --port 8080                     HTTP/WebSocket port
#   --listen-addr 0.0.0.0           bind address (127.0.0.1 for proxy-only)
```

Then open `http://<server-ip>:8080` in a browser.

### Sessions

Everything after `--` is run as the session's client app, inside the
compositor's headless Wayland display:

```sh
./waylandwebstream -- foot -e vim
```

The session is lazy: the command above isn't started at server launch, only
once the first browser connection (`/ws` or `/stream`) arrives, so an idle
server with nobody watching never runs it. It's started at most once per
server run and killed on shutdown.

If no command is given, no child process is spawned -- a Wayland client can
still be launched manually against `--display-name` as before.

## Deployment Notes

- The server just needs its HTTP port reachable from the client -- ordinary
  WebSocket traffic over TCP, no NAT traversal required.
- Authentication and TLS/`wss://` are out of scope by design: put it behind a
  reverse proxy (nginx, Caddy, etc.) the same way you would any other web
  service, and use `--listen-addr 127.0.0.1` so only the proxy can reach it.

## Testing

To run the integration tests:

```sh
# Install Node.js test dependencies (puppeteer for browser-driven testing)
cd tests && npm install && cd ..

# Run the full integration test suite
./run_integration_test.sh
```

The test suite validates the entire pipeline: compositor startup, Wayland client rendering, WebSocket/WebCodecs streaming, and screenshot validation.

## License

This project is licensed under the **GNU Affero General Public License v3.0**
(AGPL-3.0). See [LICENSE](LICENSE) for details.

Note: This project links against FFmpeg. Depending on your FFmpeg build
configuration (e.g., if built with GPL-licensed codecs like x264), the resulting
binary may be subject to GPL terms as well.
