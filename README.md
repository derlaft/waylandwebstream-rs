# WaylandWebStream

A single-binary service that runs a headless Wayland compositor and streams it
to a browser via WebRTC with low-latency, adaptive video and remote touch/input
control.

## Overview

WaylandWebStream creates a full headless Wayland environment (via
[Smithay](https://github.com/Smithay/smithay)), encodes the compositor
framebuffer to H.264 in real time (via
[FFmpeg/rust-ffmpeg](https://github.com/zmwangx/rust-ffmpeg)), and delivers the
video stream to a browser client over WebRTC (via
[rtc](https://github.com/webrtc-rs/rtc)). The user controls the remote desktop
through touch events (and later keyboard/mouse) sent back over a WebRTC data
channel and injected directly into the compositor's input pipeline.

```
┌─────────────────────────────────────────────────────┐
│                   Server (single binary)            │
│                                                     │
│  ┌──────────────┐   framebuffer   ┌──────────────┐  │
│  │   Smithay    │ ──────────────> │   FFmpeg      │  │
│  │  Headless    │                 │  H.264 enc    │  │
│  │  Compositor  │                 └──────┬───────┘  │
│  └──────┬───────┘                        │          │
│         │ inject                    RTP packets     │
│         │ input                          │          │
│  ┌──────┴───────┐                 ┌──────▼───────┐  │
│  │   Input      │ <─ data ch ──── │   WebRTC     │  │
│  │   Handler    │                 │   (rtc-rs)   │  │
│  └──────────────┘                 └──────┬───────┘  │
│                                          │          │
│  ┌──────────────┐                        │          │
│  │  Signaling   │ ◄──── HTTP ────────────┘          │
│  │  (built-in)  │                                   │
│  └──────────────┘                                   │
└──────────────────────────────────────────────────────┘
          ▲                              │
          │         Internet             │
          ▼                              ▼
┌─────────────────────────────────────────────────────┐
│                Browser Client                       │
│  ┌──────────┐  ┌───────────┐  ┌──────────────────┐  │
│  │ <video>  │  │  WebRTC   │  │  Touch/Input     │  │
│  │ element  │  │  client   │  │  event capture   │  │
│  └──────────┘  └───────────┘  └──────────────────┘  │
└─────────────────────────────────────────────────────┘
```

## Key Features

- **Single binary** -- compositor, encoder, WebRTC server, signaling, and web
  client all in one executable
- **Headless Wayland compositor** -- Smithay-based, no GPU or display required
- **Low-latency H.264 streaming** -- software encoding via FFmpeg (x264),
  tuned for real-time with `zerolatency` preset
- **Adaptive bitrate** -- automatically adjusts quality based on network
  conditions (RTCP feedback, RTT, packet loss)
- **Touch-first input** -- multi-touch events relayed from the browser and
  injected directly into the compositor (no uinput kernel round-trip)
- **WebRTC transport** -- using the sans-I/O `rtc` crate for full control over
  the event loop and timing
- **Built-in signaling** -- lightweight HTTP/WebSocket endpoint for
  offer/answer exchange; no external signaling server needed
- **Dynamic resolution** -- viewport size is negotiated per-client and can be
  changed mid-session; the compositor output, encoder, and stream adapt on
  the fly without reconnecting
- **Audio streaming** -- planned for a later phase (PipeWire capture)

## Architecture

| Component | Library | Role |
|---|---|---|
| Compositor | [smithay](https://github.com/Smithay/smithay) | Headless Wayland compositor with software rendering; dynamic output resizing |
| Video encoding | [ffmpeg-next](https://github.com/zmwangx/rust-ffmpeg) | H.264 encoding from raw framebuffer pixels |
| WebRTC | [rtc](https://github.com/webrtc-rs/rtc) | Sans-I/O WebRTC peer connection, RTP packetization |
| Signaling | built-in (hyper/axum) | HTTP + WebSocket for SDP offer/answer exchange |
| Input | direct Smithay injection | Touch/keyboard/mouse events injected into SeatState |
| Web client | embedded static HTML/JS | Minimal `<video>` + touch capture, bundled in binary |

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
- Public IP or port-forwarded UDP for WebRTC media

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
#   --stun stun:stun.l.google.com:19302
```

Then open `http://<server-ip>:8080` in a browser.

## Deployment Notes

- The server is expected to have a **public IP** with open UDP ports for
  WebRTC media traffic.
- Clients may be behind NAT -- ICE with STUN handles this.
- Adaptive bitrate adjusts for varying client connections (LAN, WiFi, mobile).
- TURN support is not included initially since the server has a public IP, but
  can be added later for edge cases.

## License

This project is licensed under the **GNU Affero General Public License v3.0**
(AGPL-3.0). See [LICENSE](LICENSE) for details.

Note: This project links against FFmpeg. Depending on your FFmpeg build
configuration (e.g., if built with GPL-licensed codecs like x264), the resulting
binary may be subject to GPL terms as well.
