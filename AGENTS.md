# AGENTS.md

Orientation for anyone (human or agent) changing this codebase. Focuses on
*why* things are the way they are and the traps that aren't obvious from the
code. For a user-facing overview see `README.md`; remaining work is in
`TODO.md`.

Note: this is a **WS + WebCodecs** system. An earlier WebRTC/RTP/ICE/STUN/TURN
implementation was fully removed — if you see references to it anywhere (old
commits, comments), it is gone, not optional.

## What it is

A single binary that runs a headless Smithay Wayland compositor, encodes its
framebuffer to H.264 with FFmpeg/x264, and ships each frame over a **binary
WebSocket** (`/stream`) to a browser that decodes it with **WebCodecs** into a
`<canvas>`. Input/resize/latency ride a second JSON WebSocket (`/ws`). The
Svelte client is built by `build.rs` and embedded with `rust-embed`. There is
no WebRTC, no RTP, no signaling negotiation, no external services.

## Architecture & data flow

```
Wayland client → compositor render() → mpsc → encoder thread (x264)
  → broadcast → /stream WS → browser VideoDecoder → canvas
browser input → /ws WS → mpsc → compositor seat injection
```

- **Two execution domains.** The compositor + render loop is **synchronous**
  and owns `CompositorState` on one thread (Smithay/calloop is not `Send`).
  Everything else (axum server, encoder forwarding, adaptive bitrate, latency)
  is **tokio async**. They communicate only through channels. The **encoder is
  its own OS thread** because FFmpeg calls block; it talks via `mpsc`/`watch`.
- **`src/main.rs`** wires all channels and runs the synchronous capture loop.
  **`src/server.rs`** is the HTTP/WS front door (`SignalingMessage` /
  `ServerMessage` are the wire protocol). **`src/compositor/state.rs`** is the
  whole compositor. **`src/encoder/mod.rs`** is the encoder thread.

## Hard-won gotchas (read before touching these areas)

### Compositor / rendering
- **`render()` bypasses Smithay's renderer.** It copies SHM buffers directly
  into a BGRA framebuffer. Consequence: no surface ever gets a scan-out output
  recorded, so `send_frames()` **must** pass `Some(Duration::ZERO)` as the
  throttle — with `None`, Smithay deems every surface "never overdue" and
  *never* sends a frame callback, so clients that wait for `frame.done` (e.g.
  `cage`) hang on their first blank buffer forever. Corollary: call
  `send_frames()` at the capture cadence, **not** every event-loop tick, or
  clients repaint far faster than we capture.
- **Hit-testing is "topmost window wins", not bbox-based.** Every window is
  configured fullscreen and `render()` scales whatever buffer the client
  attached to fill the output. `Space::element_under` would hit-test the
  literal (possibly stale/small) buffer bbox and make most of the screen
  untouchable. `surface_at()` instead maps any in-output point to the topmost
  window, scaled into its buffer space. Don't "fix" this back to bbox testing.
- **`Window::bbox()` is a cache only `Window::on_commit()` refreshes.** The
  `commit()` handler must call `window.on_commit()` or bbox stays `(0,0)` and
  the `.max(1)` fallback collapses every input target to 1×1.
- **Keyboard injection adds 8 to the evdev keycode** (`state.key()`):
  xkbcommon `Keycode` uses X11 numbering = evdev + 8. Keyboard forwards
  *physical* key identity (`KeyboardEvent.code`); the **server's XKB layout**
  resolves the character, so the browser OS layout must match it.
- **Scroll uses `AxisSource::Continuous`, not `Wheel`.** `Wheel` makes GTK
  accumulate to a ~10px notch threshold before scrolling — awful with a
  touchpad's few-px deltas. Continuous applies immediately and is fine for real
  wheels too.
- **No cursor is composited** into the framebuffer — the remote viewer sees no
  pointer. `cursor_image` is a deliberate no-op (see `TODO.md`).
- **Damage gating:** `take_dirty()` gates render+encode. A commit on an
  unpositioned surface conservatively marks the **whole output** dirty. The
  capture loop also force-renders on new-client connect and every
  `keyframe_interval` ticks (so idle screens still emit periodic resync
  keyframes and late joiners aren't stuck). `take_dirty()` must run
  unconditionally (not short-circuited by `||`) so the flag is always consumed.

### Encoder
- **Forcing a keyframe = tagging the frame `AV_PICTURE_TYPE_I`.** Resetting the
  PTS/frame counter does **nothing** — libx264 places IDRs on its own internal
  counter vs `g`/`keyint_min`. There's a regression test for this
  (`force_keyframe_actually_forces_an_idr`).
- **Control channel is drained twice** — before *and* after `blocking_recv()`.
  A `ForceKeyframe` for a just-connected client arrives back-to-back with the
  frame it targets; without the second drain it'd slip to the next frame.
- **Resolution comes from the `RawFrame`, not `EncoderConfig`.** `resize_rx`
  and `frame_rx` are separate channels with no joint ordering; the encoder can
  wake to a frame at a new size before it sees the resize. The frame-size
  mismatch check reinitializes to the frame's own dims (regression test:
  `frame_size_mismatch_reinitializes_encoder`). On resize, stale queued frames
  are drained first (they were captured at the old size).
- **Frame-buffer aliasing.** `input_frame` (BGRA) has **no owned buffer** —
  `encode_frame` points its `data[0]` straight at the `RawFrame` slice (stride
  = `width*4`, matching how `render()` packs with no row padding). `yuv_frame`
  *is* refcounted and reused across calls; that's only safe because the encoder
  has **zero frame delay** (`tune=zerolatency`, `bframes=0`, no lookahead), so
  draining `receive_packet()` to EAGAIN guarantees libx264 is done with it.
  **Turning on B-frames or lookahead breaks this** — see the safety note in
  `encode_frame`.
- **H.264 level is computed from resolution+framerate** (`select_h264_level`),
  not hardcoded, and the WebCodecs codec string (`avc1.42E0XX`) is pushed to
  clients over `/ws` on every level change. Keep `create_encoder`'s `level`
  option and `h264_codec_string` in sync.
- **`Bitrate` mode sets a VBV cap** (`vbv-maxrate`/`vbv-bufsize` ≈ 250ms) so a
  keyframe can't balloon the client jitter buffer every GOP. `Quality` (CRF)
  mode deliberately has no cap.

### Server / transport
- **`/stream` is one WebSocket message per whole frame.** Wire format (see
  `encode_video_frame`): `[type:u8][frame_id:u32 BE][has_ping:u8][ping_ts:f64
  BE][Annex-B H.264]`, header = 14 bytes. SPS/PPS are inline on every keyframe
  (`repeat_headers=1`, `annex_b=1`), so the decoder needs no `description`.
- **The video broadcast channel is intentionally tiny (cap 3).** A slow client
  should `Lagged`-skip to a recent frame and resync on the next keyframe, never
  build a backlog of stale P-frames. Don't enlarge it to "fix" lag.
- **One encoder feeds all clients.** An adaptive-bitrate cut triggered by one
  struggling client lowers quality for everyone — a property of the shared-
  encoder design, not a bug.
- **Latency uses ping-echo, no clock sync.** Client sends `ping{client_ts}`;
  the forwarding loop stamps it onto the *next* frame leaving the encoder; the
  client computes `rtt = now - echo`. The loop drains to the **latest** pending
  ping (idle screens batch them), so don't echo the oldest.
- **`network_ms` is round-trip and includes the whole server pipeline + frame
  cadence wait**, not pure transit. Adaptive bitrate only treats *decode*
  latency and *keyframe requests*/arrival-bursts as congestion — never raw RTT.

### Adaptive bitrate (`src/adaptive_bitrate.rs`)
- TCP-Reno AIMD. Primary congestion signal is the client's **keyframe-resync
  request** (decoder actually backed up) — a loss-equivalent signal, *not* a
  routine new-client connect (that path forces a keyframe directly, bypassing
  this). Secondary `ArrivalStall` catches network bufferbloat the decode-queue
  signal can't see. Decode latency can only *hold off growth*, never cut.
- All decisions live in pure `BitrateAlgorithm` (synthetic `Instant`s, no
  channels) so they're deterministically testable. Keep new logic there, not in
  the async `Controller`.

### Client (`web/`)
- **`ensureCanvasSize` re-checks every frame**, deliberately not gated by a
  one-shot "did we just resize" flag. `/ws` and `/stream` are independent
  sockets, so the first frame can arrive at the old resolution before a resize
  lands; a latch would stretch every later frame forever.
- **A dead WebCodecs decoder is permanent** — on error it goes `closed` and
  `reset()/configure()` throw. Recovery = a brand-new `VideoDecoder` instance
  then resync from a keyframe (`recoverDecoder`).
- **The client clamps resize to a hardcoded 3840×2160** because the server
  doesn't advertise (or enforce) `max_resolution` over the wire. Both halves
  are sub-16-aligned (`/16`) for H.264.
- Reconnect (auto, with backoff) is in `backoff.ts` + the socket handlers;
  `onclose` schedules it (never `onerror`, which always precedes close).
- **`decodeQueueSize > N` fires on harmless bursts.** A tab being refocused
  releases a flood of buffered frames all at once; a network clump delivers
  several P-frames within a millisecond. Both spike `decodeQueueSize` briefly
  and then drain on their own — but a naïve `> 2` threshold resyncs on both,
  freezing the picture for `KEYFRAME_FORCE_COOLDOWN` ms each time. Use a
  `BacklogTracker` with a sustained-threshold (e.g. 150ms above a soft limit)
  plus a separate hard limit for genuinely catastrophic spikes; don't resync
  on a transient that drains itself.
- **Do not request `hardwareAcceleration: 'prefer-hardware'` blindly.** On
  machines without HW decode it makes `VideoDecoder.configure()` throw, which
  hits the error callback → `recoverDecoder` → re-configure → infinite loop.
  The safe pattern: probe `VideoDecoder.isConfigSupported` for both
  `prefer-hardware` and `prefer-software` against the *real coded dimensions*
  from the first decoded frame, then upgrade the live decoder only if HW is
  confirmed. This also avoids a `no-preference` silent SW fallback — Firefox
  demonstrably picks SW intermittently under `no-preference`, giving ~70ms
  decodes for a codec+size the GPU can handle in <5ms.
- **`drawImage(VideoFrame)` can be the bottleneck, not the decoder.** On
  Firefox, a `VideoFrame → 2D canvas` blit can involve a GPU→CPU→GPU round-
  trip (readback then re-upload) and take 30–80ms — dwarfing the actual
  decode. Stamp `performance.now()` *before* `drawImage` to isolate decoder
  work from blit work; measuring after blends both into "decode latency" and
  hides the real bottleneck. Track blit time in a separate sample array and
  surface it in both the UI and the server's latency report.

## Build & test

- `cargo build` runs `build.rs` → `npm ci && npm run build` in `web/`, embeds
  `web/dist/`. It's a no-op when `web/**` is unchanged (mtime check, not just
  Cargo's per-fingerprint tracking). Missing `npm` falls back to a stale
  `web/dist/` with a warning, or panics if none exists.
- For frontend work: `cargo run` (backend :8080) + `cd web && npm run dev`
  (Vite proxies `/ws` and `/stream`, HMR against the real compositor).
- `wayland-test-client/` is a minimal Wayland client used by the integration
  tests (and handy for manual checks against `--display-name`).

## Conventions

- Keep the **wire protocol** (`SignalingMessage`/`ServerMessage` in
  `src/server.rs`, mirrored in `web/src/lib/protocol.ts`) in lockstep across
  both sides; it's the contract between binary and bundle.
- Prefer adding CLI knobs in `src/config.rs` (clap) over hardcoding.
- This project links FFmpeg/x264 and is **AGPL-3.0**; the resulting binary may
  carry GPL terms depending on the FFmpeg build.
</content>
</invoke>
