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
- **Touch-first input** -- multi-touch events relayed from the browser and
  injected directly into the compositor (no uinput kernel round-trip)
- **WebSocket + WebCodecs transport** -- one binary WebSocket per frame, no
  SDP/ICE negotiation; the browser's native `VideoDecoder` does the decoding
- **Built-in HTTP/WebSocket server** -- serves the web client, the binary
  video stream, and the input/control channel from a single built-in server;
  no external signaling or STUN/TURN infrastructure needed
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
| Input | direct Smithay injection | Touch/keyboard/mouse events injected into SeatState |
| Web client | embedded static HTML/JS | `<canvas>` + WebCodecs decode, touch capture, bundled in binary |

## Requirements

### Build dependencies

- Rust 1.75+ (2024 edition)
- FFmpeg development libraries (libavcodec, libavformat, libavutil, libswscale)
- Wayland development libraries (libwayland-server)
- pkg-config

### Runtime

- Linux (Wayland is Linux-only)
- FFmpeg shared libraries
- No GPU required (software rendering + software encoding)
- A reachable TCP port for the HTTP/WebSocket server (no UDP, STUN, or TURN
  needed -- it's a plain WebSocket connection)

## Building

```sh
cargo build --release
```

## Usage

```sh
# Start the server (default: 1280x720, port 8080 for signaling)
./waylandwebstream

# Options (planned)
#   --initial-resolution 1280x720   (default for new clients)
#   --max-resolution 3840x2160      (upper bound for client-requested resize)
#   --port 8080
```

Then open `http://<server-ip>:8080` in a browser.

## Deployment Notes

- The server just needs its HTTP port reachable from the client -- ordinary
  WebSocket traffic, no NAT traversal, ICE, STUN, or TURN required.
- Put it behind a reverse proxy (e.g. for TLS/`wss://` or authentication) the
  same way you would any other web service.

## Testing

To run the integration tests:

```sh
# Install Node.js test dependencies (puppeteer for browser-driven testing)
cd tests && npm install && cd ..

# Run the full integration test suite
./run_integration_test.sh
```

The test suite validates the entire pipeline: compositor startup, Wayland client rendering, WebSocket/WebCodecs streaming, and screenshot validation.

### Development Guidelines

**Important notes for AI assistants and contributors:**

* **When you need a common system tool, ask the user to install it** (instead of giving up instantly, trying 10 other tools, then giving up and writing a script/tool yourself)
* **Avoid this situation specifically:** changing tooling from the best option just because something did not work or took too long the first time you tried
* Be patient with package installations (like puppeteer downloading Chrome) - they may take time but are necessary
* Use the right tool for the job, even if it requires user intervention to install dependencies

## License

This project is licensed under the **GNU Affero General Public License v3.0**
(AGPL-3.0). See [LICENSE](LICENSE) for details.

Note: This project links against FFmpeg. Depending on your FFmpeg build
configuration (e.g., if built with GPL-licensed codecs like x264), the resulting
binary may be subject to GPL terms as well.
