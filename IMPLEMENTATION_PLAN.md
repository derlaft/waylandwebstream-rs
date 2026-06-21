# Implementation Plan

This document describes the phased implementation plan for WaylandWebStream.

---

## Guiding Principles

1. **Low latency above all** -- every design choice prioritizes minimizing
   end-to-end latency (capture-to-display).
2. **Dynamic viewport** -- resolution is not fixed at startup; it is
   negotiated per-client and can change mid-session. Every layer
   (compositor, encoder, RTP, input mapping) must handle resolution changes
   gracefully without requiring a reconnect.
3. **Async where possible** -- use tokio as the async runtime; keep FFmpeg
   encoding on a dedicated thread (it is synchronous/blocking) and communicate
   via channels.
4. **Single binary** -- no external services, no separate signaling server, no
   separate web server.
5. **Incremental delivery** -- each phase produces a working, testable artifact.

---

## Technology Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Compositor | Smithay (headless backend) | Mature Rust Wayland library; headless mode avoids GPU/display dependency |
| WebRTC | `rtc` crate (sans-I/O) | Full control over event loop timing; no hidden async tasks; runtime-agnostic |
| Video encoding | `ffmpeg-next` (x264 software) | Universal H.264 support; `zerolatency` tune; hardware encoding can be added later |
| Input injection | Direct Smithay `SeatState` | No kernel round-trip; lowest latency; compositor already owns the seat |
| Signaling | Built-in HTTP + WebSocket | Single binary requirement; minimal dependency; axum or warp |
| Audio | Deferred to Phase 5 | Focus on video + input first |
| Async runtime | tokio | Standard in Rust ecosystem; rtc crate is runtime-agnostic so no conflict |
| Web client | Embedded HTML/JS | `include_str!` / `include_bytes!` in binary; no build toolchain for frontend |

---

## Phase 0: Project Skeleton

**Goal:** Cargo workspace compiles, CI-ready structure.

### Tasks

- [ ] Initialize Cargo project (single binary crate, not workspace -- keep it
      simple initially)
- [ ] Set up `Cargo.toml` with initial dependencies:
  - `smithay` (features: `wayland_frontend`, `renderer_pixman` for software
    rendering)
  - `ffmpeg-next`
  - `rtc` + `rtc-media` + `rtc-rtp` + `rtc-rtcp`
  - `tokio` (features: `full`)
  - `axum` (for HTTP/WS signaling)
  - `tracing` + `tracing-subscriber` (structured logging)
  - `clap` (CLI argument parsing)
  - `serde` + `serde_json` (SDP/signaling messages)
  - `bytes`
- [ ] Create module structure:
  ```
  src/
    main.rs           -- entry point, CLI, tokio runtime bootstrap
    compositor/
      mod.rs          -- Smithay headless compositor setup
      state.rs        -- compositor state (surfaces, outputs, seat)
      input.rs        -- input event injection
    encoder/
      mod.rs          -- FFmpeg encoder setup and frame encoding
      frame.rs        -- framebuffer capture and pixel format conversion
    webrtc/
      mod.rs          -- rtc peer connection management
      session.rs      -- per-client WebRTC session
      signaling.rs    -- HTTP/WS signaling server (offer/answer/ICE)
      rtp.rs          -- RTP packetization for H.264
    input/
      mod.rs          -- browser event deserialization
      touch.rs        -- touch event handling and coordinate mapping
      keyboard.rs     -- keyboard event handling (later)
      mouse.rs        -- mouse/pointer event handling (later)
    web/
      mod.rs          -- embedded static file serving
      client.html     -- minimal HTML/JS WebRTC client
    config.rs         -- runtime configuration (initial resolution, max
                         resolution, port, STUN, etc.)
  ```
- [ ] Add AGPL-3.0 `LICENSE` file
- [ ] Verify `cargo check` passes

### Deliverable

Compiling skeleton with empty module stubs.

---

## Phase 1: Headless Compositor

**Goal:** A running Smithay headless compositor that accepts Wayland client
connections and renders to an in-memory framebuffer.

### Tasks

- [ ] Set up Smithay with the headless backend
- [ ] Create a virtual output (e.g., 1280x720 @ 60fps, configurable)
- [ ] Use the Pixman software renderer to composite surfaces into a buffer
- [ ] Set up a Wayland socket so applications can connect (e.g.,
      `WAYLAND_DISPLAY=wayland-wws-0`)
- [ ] Implement basic compositor state:
  - `CompositorState`, `ShmState`, `OutputState`, `SeatState`
  - Surface commit handling (wl_surface, xdg_shell basics)
- [ ] Render loop: on each frame, composite all mapped surfaces into the
      output framebuffer
- [ ] Export the framebuffer as a `&[u8]` (RGBA/BGRA pixel data) for the
      encoder
- [ ] **Dynamic output resizing:**
  - Implement `resize_output(width, height)` method on compositor state
  - Change the Smithay output mode (resolution) at runtime via
    `Output::change_current_state()` -- this sends `wl_output.mode` and
    `wl_output.done` events to connected Wayland clients, causing them to
    resize their surfaces
  - Reallocate the Pixman render buffer to the new dimensions
  - Enforce a configurable maximum resolution (e.g., `--max-resolution
    3840x2160`) to bound memory and CPU usage
  - Emit a resize event on a channel so the encoder and WebRTC layers can
    react

### Key Smithay APIs

- `smithay::backend::headless::HeadlessBackend`
- `smithay::backend::renderer::pixman::PixmanRenderer`
- `smithay::wayland::compositor::CompositorState`
- `smithay::wayland::shell::xdg::XdgShellState`
- `smithay::wayland::seat::SeatState`

### Testing

- Launch a simple Wayland client (e.g., `weston-simple-egl` or
  `weston-terminal`) against the compositor and verify it renders to the
  framebuffer (dump as PNG for visual verification).

### Deliverable

A running compositor that renders client windows to an in-memory buffer.

---

## Phase 2: Video Encoding Pipeline

**Goal:** Take framebuffer pixels and produce H.264 NAL units suitable for RTP
packetization.

### Tasks

- [ ] Initialize FFmpeg encoder context:
  - Codec: `libx264`
  - Pixel format: convert from compositor output (likely BGRA) to `YUV420P`
    via `swscale`
  - Tune: `zerolatency`
  - Preset: `ultrafast` (can be adjusted based on CPU budget)
  - Profile: `baseline` or `constrained_baseline` (maximum browser compat)
  - Bitrate: start at 2 Mbps, adjustable
  - Keyframe interval: every 2 seconds (tunable; lower for faster seek/recovery)
  - No B-frames (latency)
- [ ] Run encoder on a **dedicated thread** (FFmpeg calls are blocking):
  - Receive raw frames via `tokio::sync::mpsc` channel
  - Send encoded packets (NAL units) back via another channel
- [ ] Frame capture flow:
  1. Compositor render callback fires
  2. Read pixels from Pixman renderer output
  3. Send pixel buffer to encoder thread
  4. Encoder produces `AVPacket` (H.264 NAL units)
  5. Forward to WebRTC layer
- [ ] Implement frame pacing:
  - Target 30fps initially (configurable)
  - Skip frames if encoder falls behind (drop oldest unprocessed frame)
  - Track encode latency metrics
- [ ] Handle pixel format conversion efficiently:
  - Use `swscale` for BGRA -> YUV420P
  - Reuse allocated `SwsContext` across frames
  - Consider `SWS_FAST_BILINEAR` for speed
- [ ] **Dynamic resolution support (encoder reinit):**
  - Listen for resize events from the compositor (via a watch/broadcast
    channel carrying `(width, height)`)
  - On resize: drain any in-flight frames, then tear down and recreate:
    - The `AVCodecContext` (x264 does not support resolution change without
      reinit)
    - The `SwsContext` (input dimensions changed)
    - Pre-allocated frame buffers
  - Emit an IDR frame immediately after reinit so the decoder can
    resynchronize without waiting for the next keyframe interval
  - Width and height must be even (H.264 requirement) -- round up if
    necessary
  - This operation is ~5-20ms; during reinit, incoming frames are dropped

### Performance Considerations

- Pre-allocate frame buffers (avoid allocation per frame); reallocate only on
  resize
- Use frame dropping to prevent encoder queue buildup
- The encoder thread should signal backpressure via a bounded channel

### Testing

- Feed synthetic frames (solid colors, gradients) and verify H.264 output is
  decodable
- Measure encoding latency per frame

### Deliverable

Working encoding pipeline: pixels in, H.264 packets out.

---

## Phase 3: WebRTC Streaming

**Goal:** Stream the H.264 video to a browser via WebRTC.

### Tasks

#### Signaling Server

- [ ] HTTP endpoint (`POST /offer`) -- receives browser SDP offer, returns
      server SDP answer
- [ ] WebSocket endpoint (`/ws`) -- for trickle ICE candidate exchange
- [ ] Serve the embedded HTML/JS client at `GET /`
- [ ] ICE configuration:
  - Include at least one public STUN server (e.g.,
    `stun:stun.l.google.com:19302`)
  - Server-side: bind to public IP, advertise host candidates
  - No TURN initially (server has public IP)

#### WebRTC Session

- [ ] For each connecting client, create an `RTCPeerConnection` via the `rtc`
      crate
- [ ] Add a video track (H.264, clock rate 90000)
- [ ] Create a data channel for input events (`input-events`, reliable +
      ordered)
- [ ] Create a data channel for control messages (`control`, reliable +
      ordered) -- used for resize requests, resolution confirmations, and
      other session-level negotiation
- [ ] Drive the sans-I/O event loop:
  ```
  loop {
      // 1. poll_write -> send UDP packets
      // 2. poll_event -> handle state changes
      // 3. poll_read  -> receive data channel messages (input events)
      // 4. poll_timeout -> schedule next timer
      // 5. handle_read <- incoming UDP
      // 6. handle_timeout <- timer expired
      // 7. handle_write <- new encoded video packets to send
  }
  ```
- [ ] Integrate the event loop with tokio (using `tokio::select!` over UDP
      socket, timer, and incoming encoded packets)

#### RTP Packetization

- [ ] Packetize H.264 NAL units into RTP packets
  - Use H.264 RTP payload format (RFC 6184)
  - Handle NAL unit fragmentation (FU-A) for large frames
  - Set RTP timestamps from frame PTS (90kHz clock)
  - Set marker bit on last packet of each frame

#### Dynamic Resolution Negotiation

- [ ] Handle resize requests from the client via the `control` data channel:
  ```json
  {
    "type": "resize",
    "width": 1920,
    "height": 1080
  }
  ```
- [ ] Server-side resize flow:
  1. Validate requested dimensions (within `--max-resolution` bounds, both
     dimensions even, minimum 320x240)
  2. Trigger compositor `resize_output()` (updates Wayland output mode,
     Wayland clients resize their surfaces)
  3. Compositor emits resize event to encoder thread
  4. Encoder reinitializes (new `AVCodecContext` + `SwsContext`)
  5. Encoder emits IDR frame at new resolution
  6. Server sends confirmation back to client via `control` channel:
     ```json
     {
       "type": "resize_ack",
       "width": 1920,
       "height": 1080
     }
     ```
  7. Browser decoder handles the resolution change transparently (H.264
     decoders handle mid-stream resolution changes when a new SPS/PPS +
     IDR arrives)
- [ ] No SDP renegotiation is needed -- H.264 resolution is not fixed in
      SDP; the RTP stream simply starts carrying a different resolution
      after the IDR frame
- [ ] Handle the transient period: between resize request and the first
      frame at the new resolution, the client may see a brief stall (~20-50ms);
      this is acceptable
- [ ] Client may also request resize on:
  - Initial connection (send desired resolution before first frame)
  - Browser window resize / orientation change
  - Device pixel ratio change (e.g., moving browser between monitors)

#### Adaptive Bitrate

- [ ] Monitor RTCP Receiver Reports from the client:
  - Packet loss ratio
  - RTT (round-trip time)
  - Jitter
- [ ] Implement a simple adaptive algorithm:
  - High loss (>5%) or high RTT (>200ms) -> reduce bitrate
  - Low loss (<1%) and low RTT (<50ms) -> increase bitrate
  - Bitrate range: 500 Kbps -- 8 Mbps
  - Adjust by changing encoder bitrate (requires sending new params to encoder
    thread) or by changing resolution/framerate
- [ ] React to PLI (Picture Loss Indication) -- force an IDR frame

### Testing

- Connect from Chrome/Firefox and verify video plays
- Simulate network degradation (tc/netem) and verify bitrate adapts
- Measure glass-to-glass latency

### Deliverable

Browser can connect and see the compositor output as a live video stream.

---

## Phase 4: Input Handling (Touch First)

**Goal:** Remote user can control the compositor via touch events from the
browser.

### Tasks

#### Browser Side (client.html)

- [ ] Capture touch events (`touchstart`, `touchmove`, `touchend`,
      `touchcancel`) on the `<video>` element
- [ ] Normalize coordinates to [0.0, 1.0] range (relative to video dimensions)
- [ ] Serialize events as JSON over the data channel:
  ```json
  {
    "type": "touch",
    "action": "start|move|end|cancel",
    "touches": [
      {"id": 0, "x": 0.5, "y": 0.3},
      {"id": 1, "x": 0.7, "y": 0.8}
    ],
    "timestamp": 1234567890
  }
  ```
- [ ] Prevent default browser touch behavior (zooming, scrolling)
- [ ] **Dynamic resize from browser:**
  - On `window.resize` / `orientationchange` / initial load, measure the
    available viewport size and device pixel ratio
  - Send a `resize` message on the `control` data channel with the desired
    compositor resolution (viewport pixels * devicePixelRatio, or a policy
    the server defines)
  - Wait for `resize_ack` before updating coordinate mapping (the
    normalized [0,1] coordinates are resolution-independent, so touch input
    continues working during the transition without special handling)

#### Server Side

- [ ] Receive data channel messages in the WebRTC event loop
- [ ] Deserialize touch events
- [ ] Map normalized coordinates to compositor pixel coordinates (multiply by
      **current** output resolution -- this is always correct because the
      compositor owns the authoritative resolution and the client uses [0,1]
      normalized coordinates)
- [ ] Inject touch events into Smithay's `SeatState`:
  - Create touch down/motion/up/cancel events
  - Route to the correct surface based on coordinates (hit testing via
    Smithay's `under_from_surface_tree`)
  - Handle multi-touch (multiple simultaneous touch points with distinct IDs)
- [ ] Ensure proper Wayland touch event sequencing:
  - `wl_touch.down` -> `wl_touch.motion` (0..n) -> `wl_touch.up`
  - `wl_touch.frame` after each batch

#### Later: Keyboard and Mouse

- [ ] Keyboard: capture `keydown`/`keyup` in browser, map to Linux keycodes
      (or use `KeyboardEvent.code`), inject via `SeatState::keyboard`
- [ ] Mouse/pointer: capture `mousemove`, `mousedown`, `mouseup`,
      `wheel`; inject via `SeatState::pointer`
- [ ] These are lower priority than touch

### Testing

- Open a touch-enabled app in the compositor (e.g., a GTK4 app)
- Touch the video in the browser and verify the app receives the events
- Test multi-touch gestures (pinch, two-finger scroll)

### Deliverable

Full interactive remote desktop with touch input.

---

## Phase 5: Audio Streaming (Future)

**Goal:** Stream audio output from compositor applications to the browser.

### Tasks

- [ ] Set up PipeWire (or PulseAudio) monitor source to capture application
      audio output
- [ ] Encode audio with Opus codec (via FFmpeg or a dedicated Opus crate)
- [ ] Add an audio track to the WebRTC peer connection
- [ ] Packetize Opus frames as RTP and send alongside video
- [ ] Handle audio/video synchronization (RTCP SR timestamps)
- [ ] Browser plays audio via the `<video>` element (WebRTC handles this
      natively)

### Considerations

- PipeWire is the modern standard; PulseAudio compatibility layer is available
- Audio capture should be in its own async task
- Buffer sizing is critical for low latency (target 10-20ms audio frames)

---

## Phase 6: Polish and Hardening (Future)

### Tasks

- [ ] Multi-client support (multiple simultaneous viewers)
  - Shared video stream (single encode, multiple RTP senders)
  - Input arbitration (one controller at a time, or collaborative)
- [ ] Connection lifecycle management
  - Graceful disconnect handling
  - Automatic reconnection from browser
  - ICE restart on network change
- [ ] Monitoring and metrics
  - Prometheus-compatible metrics endpoint
  - Encode latency, bitrate, packet loss, active connections
- [ ] Security
  - DTLS is built into WebRTC (encryption in transit)
  - Optional authentication for the signaling endpoint
  - Rate limiting
- [ ] Containerization
  - Dockerfile with FFmpeg + Wayland libs
  - Minimal base image
- [ ] Hardware encoding support (VAAPI, NVENC)
  - Detect available hardware encoders at startup
  - Fall back to software encoding

---

## Module Dependency Graph

```
main.rs
  ├── config.rs
  ├── compositor/
  │     ├── state.rs      (Smithay state, surfaces, output, dynamic resize)
  │     └── input.rs      (touch/keyboard/mouse injection)
  ├── encoder/
  │     ├── mod.rs         (encoder thread, channel setup)
  │     └── frame.rs       (pixel capture, swscale conversion)
  ├── webrtc/
  │     ├── session.rs     (RTCPeerConnection lifecycle)
  │     ├── signaling.rs   (HTTP/WS server, SDP exchange)
  │     ├── rtp.rs         (H.264 RTP packetization)
  │     └── control.rs     (control data channel: resize, session messages)
  ├── input/
  │     ├── touch.rs       (touch event parsing + coordinate mapping)
  │     ├── keyboard.rs    (keyboard event parsing, future)
  │     └── mouse.rs       (mouse event parsing, future)
  └── web/
        ├── mod.rs          (static file server)
        └── client.html     (embedded browser client)
```

## Data Flow

### Frame Encoding Path (steady state)

```
 Compositor render loop (16ms @ 60fps or 33ms @ 30fps)
    │
    ▼
 Read pixel buffer from Pixman renderer (current resolution)
    │
    ▼
 Send to encoder thread (bounded mpsc channel, drop-on-full)
    │
    ▼
 [Encoder thread]
 BGRA → YUV420P (swscale) → x264 encode → H.264 NAL units
    │
    ▼
 Send encoded packets back to WebRTC task (mpsc channel)
    │
    ▼
 [WebRTC task]
 NAL → RTP packetization → RTCPeerConnection.handle_write()
    │
    ▼
 poll_write() → UDP send to client
    │
    ▼
 [Browser]
 WebRTC → decode H.264 (hardware) → display in <video>
```

### Resize Path

```
 Browser detects viewport change (window resize, orientation, initial connect)
    │
    ▼
 Sends {"type":"resize","width":W,"height":H} on control data channel
    │
    ▼
 [WebRTC task] validates dimensions, forwards to compositor
    │
    ▼
 [Compositor] Output::change_current_state(new mode)
    │  ├── Wayland clients receive wl_output.mode + done → resize surfaces
    │  └── Pixman render buffer reallocated
    │
    ▼
 Emits resize event (broadcast channel) → encoder thread receives it
    │
    ▼
 [Encoder thread] drains queue, reinits AVCodecContext + SwsContext
    │
    ▼
 First frame at new resolution: IDR (SPS/PPS + keyframe)
    │
    ▼
 Browser H.264 decoder picks up new SPS → seamless resolution switch
    │
    ▼
 Server sends {"type":"resize_ack","width":W,"height":H} to browser
```

## Latency Budget Target

| Stage | Target | Notes |
|---|---|---|
| Framebuffer capture | <1ms | Memory copy |
| Pixel format conversion | 1-3ms | swscale BGRA→YUV420P |
| H.264 encoding | 3-10ms | x264 ultrafast/zerolatency, depends on resolution |
| RTP packetization | <1ms | |
| Network transit | 5-50ms | Depends on client location |
| Browser decode | 1-5ms | Hardware H.264 decode |
| Display | ~8ms | 1 frame at 120Hz |
| **Total** | **~20-80ms** | Glass-to-glass |
| **Resize transient** | **~30-100ms** | Encoder reinit + IDR; frames dropped during reinit |

## Adaptive Bitrate Strategy

```
                ┌──────────────────────┐
                │   RTCP Receiver      │
                │   Reports from       │
                │   browser            │
                └──────────┬───────────┘
                           │
                    ┌──────▼──────┐
                    │  Analyze:   │
                    │  - loss %   │
                    │  - RTT      │
                    │  - jitter   │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
         loss > 5%    1% < loss    loss < 1%
         RTT > 200ms   < 5%       RTT < 50ms
              │            │            │
              ▼            ▼            ▼
         Decrease      Hold         Increase
         bitrate       steady       bitrate
         (÷ 1.5)                    (× 1.2)
              │            │            │
              ▼            ▼            ▼
         Also consider: reduce framerate (30→15fps)
         or server-initiated resolution downscale (1080p→720p)
         under severe degradation. Resolution downscale uses
         the same resize pipeline (compositor → encoder reinit
         → IDR) but is triggered by the server's ABR logic
         rather than a client request.
```

## Open Questions / Future Decisions

1. **Frame capture mechanism:** Smithay's Pixman renderer gives us direct
   buffer access. If GPU rendering is desired later (e.g., for OpenGL clients),
   we would need `glReadPixels` or a DMA-BUF approach -- but software rendering
   is sufficient for the initial implementation.

2. **Multi-client video:** Encoding once and sending to multiple peers via
   separate RTP streams is straightforward. The question is whether different
   clients should get different quality levels (simulcast-like behavior from
   the server).

3. **Cursor rendering:** The compositor cursor may need to be composited into
   the framebuffer (since WebRTC video is a single stream). Alternatively, a
   separate cursor overlay could be sent via data channel coordinates.

4. **Window management:** The headless compositor needs basic window management
   (stacking, focus). A simple fullscreen-by-default policy may suffice
   initially.

5. **Multi-client resize conflicts:** When multiple clients are connected,
   they may request different resolutions. Options:
   - **Single-master:** only the "controlling" client can resize; others
     receive whatever resolution is current.
   - **Largest-wins:** compositor uses the largest requested resolution;
     smaller clients receive a downscaled stream (either via swscale in a
     per-client encoder, or the client scales the `<video>` element via CSS).
   - **Per-client virtual output:** each client gets its own Smithay output
     at its requested resolution. This is the most flexible but multiplies
     encoding cost.
   - For Phase 3-4 (single client), this is not a concern. Decision deferred
     to Phase 6.

6. **Resize rate limiting:** Rapid resize events (e.g., user dragging a
   browser window edge) would cause repeated encoder reinits. A debounce
   (e.g., 200-500ms after the last resize event) should be applied on the
   client side before sending the resize request. The server should also
   reject resize requests that arrive faster than one per second.
