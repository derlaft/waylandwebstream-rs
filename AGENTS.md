# AGENTS.md

Orientation for anyone (human or agent) changing this codebase. Focuses on
*why* things are the way they are and the traps that aren't obvious from the
code. For a user-facing overview see `README.md`; remaining work is in
`TODO.md`, and the performance/latency backlog (with rationale for what was
done vs. deliberately deferred) is in `PERF_TODO.md`.

Note: this is a **WS + WebCodecs** system. An earlier WebRTC/RTP/ICE/STUN/TURN
implementation was fully removed — if you see references to it anywhere (old
commits, comments), it is gone, not optional.

## What it is

A single binary that runs a headless Smithay Wayland compositor, encodes its
framebuffer to H.264 with FFmpeg/x264, and ships each frame over a **binary
WebSocket** to a browser that decodes it with **WebCodecs** into a `<canvas>`.
Audio is captured from PipeWire and Opus-encoded. The Svelte client is built by
`build.rs` and embedded with `rust-embed`. There is no WebRTC, no RTP, no
signaling negotiation, no external services.

Everything rides a **single `/client` WebSocket** that multiplexes video,
audio, and control (input/resize/latency/**clipboard**) in one connection,
framed by `src/proto.rs` (8-byte header + payload; `MSG_*` type bytes). The old
split endpoints (`/stream`, `/audio`, `/ws`) were removed — `/client` is the
only one.

## Architecture & data flow

```
Wayland client → compositor render() → mpsc → encoder thread (x264)
  → broadcast → /client WS → browser VideoDecoder → canvas
PipeWire → audio thread (Opus) → broadcast → /client WS → browser
browser input/clipboard → /client WS → mpsc → compositor seat injection
nested-compositor selection ↔ data-control client (src/clipboard.rs) ↔ browser
```

An **optional GL/EGL compositor + VA-API encoder** path also exists
(`--compositor gl` / `--encoder vaapi`, with linux-dmabuf import); the
SHM-copy + x264 path described below is the default and what runs on the
GPU-less dev box.

- **Two execution domains.** The compositor + render loop is **synchronous**
  and owns `CompositorState` on one thread (Smithay/calloop is not `Send`).
  Everything else (axum server, encoder forwarding, adaptive bitrate, latency)
  is **tokio async**. They communicate only through channels. The **encoder is
  its own OS thread** because FFmpeg calls block; it talks via `mpsc`/`watch`.
- **`src/main.rs`** wires all channels and runs the synchronous capture loop.
  **`src/server.rs`** is the HTTP/WS front door (`SignalingMessage` /
  `ServerMessage` are the wire protocol). **`src/compositor/state.rs`** is the
  whole compositor. **`src/encoder/mod.rs`** is the encoder thread.
  **`src/audio.rs`** is the PipeWire→Opus thread, **`src/session.rs`** spawns
  the `-- <cmd>` app and discovers a nested compositor's socket, and
  **`src/clipboard.rs`** is the data-control clipboard bridge.

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
- **Capture is damage/input-driven, not grid-locked.** The loop renders as soon
  as there's damage (`is_dirty()` peeks it non-consumingly; `take_dirty()` still
  does the actual consume) *and* one `frame_interval` has elapsed since the last
  capture (`last_capture`) — it does **not** wait for the periodic `next_frame`
  deadline. The periodic tick still fires so `send_frames()` and the idle
  keyframe cadence keep ticking on a static screen. Reverting to a grid-only
  gate puts ~½ frame of latency back on every interactive update.
- **Input does not wake the loop by itself.** The touch/mouse/key/resize
  channels are tokio `mpsc`, which calloop can't poll, so the async handlers
  fire a `calloop::ping::Ping` (`SignalingState::wake_input_loop`) to break
  `dispatch()` out the instant an event is queued; input is then drained right
  after `dispatch_clients` so the resulting seat events flush in the same
  iteration. Without the ping, input waits out the dispatch timeout (~½ frame).

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
  clients over `/client` on every level change. Keep `create_encoder`'s `level`
  option and `h264_codec_string` in sync.
- **`Bitrate` mode sets a VBV cap** (`vbv-maxrate`/`vbv-bufsize` ≈ 250ms) so a
  keyframe can't balloon the client jitter buffer every GOP. `Quality` (CRF)
  mode deliberately has no cap.
- **There is no in-place bitrate change.** libavcodec (via `ffmpeg-next`)
  exposes no runtime rate-control reconfig for libx264 *or* `h264_vaapi`, so
  `change_bitrate` tears the encoder down and rebuilds it — and a fresh encoder
  always emits an IDR on its first frame. The `frame_count = 0` reset does
  **not** force that IDR (libx264 places IDRs on its own counter; same as
  `ForceKeyframe`) — the rebuild does. Don't re-add a "reset counter to force
  IDR" comment, and don't expect `set_bit_rate` after `open` to take effect. The
  rebuild's IDR is unavoidable, so the cost is controlled by *coalescing* how
  often the rate actually changes (see Adaptive bitrate).
- **The encoder skips to the newest queued frame** (`skip_to_newest_frame`,
  right after `blocking_recv`). If it fell behind (a heavy IDR, a CPU spike, a
  bitrate rebuild), frames pile in `frame_rx` (cap 4); only the latest matters
  for a live stream, so the rest are dropped and their buffers returned for
  reuse (like the resize-drain). Intentional, not a frame-dropping bug.
- **Damage-aware BGRA→YUV has a hard invariant: every changed row must reach the
  encoder, or you get *persistent* corruption** (old pixels in a region until it
  next changes). The encoder converts only the rows in `RawFrame::damage` into a
  **persistent** `yuv_frame` reused across calls (`convert_damaged_rows`,
  via `sws_scale` with `srcSliceY=0` + plane pointers offset to the band — the
  slice API rejects a mid-frame `srcSliceY`). So any path that loses a frame's
  damage must carry it forward: `skip_to_newest_frame` **unions** skipped frames'
  damage onto the kept frame; the capture loop **`readd_damage`s** a frame
  dropped at the bounded send queue; and **every keyframe forces a full convert**
  (`force_keyframe ⇒ scaler.run`) as a self-healing backstop so any gap clears
  within a GOP and every IDR is whole. Corollary that bit once: the encoder
  handoff buffer (`render()`'s output) **must be a full copy of the canvas**, not
  just the damaged rows — the skip-union reads *other* frames' rows out of the
  kept buffer, so a partial copy leaves them stale (this is why the "Stage C"
  partial handoff was reverted; a correct one needs per-buffer damage history).
  The GL path leaves `damage` empty → full convert. `DamageRect` is a row band
  `{y, height}`; the encoder is whole-row (chroma is vertically 2x-subsampled).

### Hardware acceleration (optional GL compositor / VAAPI encoder)

Behind `--compositor gl` / `--encoder vaapi` (defaults `sw`/`x264`; missing
backends warn and fall back). The pipeline is composable: `Box<dyn Compositor>`
(`SwCompositor` / `GlCompositor`) × `Box<dyn VideoEncoder>` (`X264Encoder` /
`VaapiEncoder`) over a `CapturedFrame::{Cpu(RawFrame), Gpu{dmabuf,..}}` channel.
The dev box (s8) has **no `/dev/dri`**, so this path is software-only there and
was validated on separate Intel HD Graphics hardware.

FFmpeg/VAAPI traps (each cost real debugging):
- **Borrowed vs owned hwframes ctx.** `av_buffersink_get_hw_frames_ctx` returns
  a *borrowed* pointer (unlike the similarly named `avfilter_link_get_hw_frames_ctx`,
  which refs). Unreffing it corrupts the filtergraph's link state → a
  double-free/heap abort, often surfacing only at process exit. Take a fresh
  `av_buffer_ref` for the encoder's `hw_frames_ctx` and never touch the
  original — that ref is load-bearing, not defensive copying.
- **HW filter nodes must split alloc from init.** The `buffer` source (hw
  `pix_fmt`), `hwupload`, and `scale_vaapi` each validate
  `hw_device_ctx`/`hw_frames_ctx` in their own `init()`; setting it after
  `Graph::parse()`/`add` is too late ("a hardware device reference is
  required…" / "requires hw_frames_ctx non-NULL"). Use
  `avfilter_graph_alloc_filter` + set the ctx + `avfilter_init_dict`, and attach
  the source's frames ctx via `av_buffersrc_parameters_set`. Name nodes
  directly; don't rely on autogenerated `Parsed_*` names.
- **ffmpeg 8.x uses `Pixel::VAAPI`, not `Pixel::VAAPI_VLD`** (legacy
  `ff_api_vaapi`-only).
- **`h264_vaapi` has no `forced_idr`** on this build — force a keyframe by
  tagging `pict_type = AV_PICTURE_TYPE_I` (same mechanism as x264).
- **Free the device + frames ctx on teardown AND on every resolution rebuild**,
  or you leak a VA surface pool per resize.

GL compositor specifics:
- `GlCompositor` (`src/compositor/gl.rs`) is a separate `Compositor` impl, not a
  rewrite of `state.rs`'s `render()` (the SW memcpy path is untouched). It uses
  `space_render_elements`/`render_output` and **does not** stretch a
  non-matching client buffer to fill the output the way SW does (intentional).
- **linux-dmabuf needs the v4/v5 *feedback* global**, not v3 formats-only:
  Mesa's wayland-egl needs the `main_device` event (render node `st_rdev`) or
  real GL clients can't pick a driver.
- The renderer is shared as `Rc<RefCell<GlesRenderer>>` (single-threaded loop)
  so both `DmabufHandler` and the compositor reach it.
- **Zero-copy** (`--compositor gl --encoder vaapi` only): `GlCompositor` emits
  `Gpu` dmabuf frames for that exact pairing (`EncoderConfig::gpu_frames`);
  every other combination uses `ExportMem` CPU readback. A GL `sync.wait()`
  fence precedes handing the dmabuf to VAAPI (different API, no implicit GL
  ordering).

### Server / transport
- **`/client` is the only endpoint** (the legacy `/ws`, `/stream`, `/audio` were
  removed). It multiplexes everything over `src/proto.rs` framing: 8-byte header
  `[msg_type:u8][flags:u8][reserved:u16][len:u32 LE]` + payload.
  `MSG_VIDEO_FRAME`/`MSG_AUDIO_FRAME`/`MSG_CONTROL` go server→client;
  `MSG_CLIENT_MSG` (JSON `SignalingMessage`) and
  `MSG_CLIPBOARD_IMAGE`/`MSG_CLIENT_CLIPBOARD_IMAGE` (binary clipboard images)
  go both ways. `ServerMessage::Cursor`/`Bitrate`/`Codec`/`Clipboard` ride
  `MSG_CONTROL` as JSON. The TS side mirrors it in `web/src/lib/protocol.ts` —
  keep the two in lockstep byte-for-byte.
- **Video payload**: `[frame_id:u32 BE][ping_echo:f64 BE][capture_to_encode_ms:
  f64 BE][Annex-B H.264]`; `is_keyframe`/`has_ping` are flags in the header
  byte. SPS/PPS are inline on every keyframe (`repeat_headers=1`, `annex_b=1`),
  so the decoder needs no `description`.
- **Audio**: PipeWire loopback → Opus (96 kbps, 20 ms) on a dedicated thread
  (`src/audio.rs`), broadcast like video. `audio_tx` is `None` when PipeWire
  fails to start — audio frames just never appear, never an error.
- **The video broadcast channel is intentionally tiny (cap 3).** A slow client
  should `Lagged`-skip to a recent frame and resync on the next keyframe, never
  build a backlog of stale P-frames. Don't enlarge it to "fix" lag.
- **The broadcast carries `Arc<EncodedPacket>`, not `EncodedPacket`.**
  `tokio::broadcast` clones the stored value on every `recv()`, and an
  `EncodedPacket` clone deep-copies its whole H.264 buffer — wasteful even for
  the single client. `Arc` makes `recv()` a refcount bump; the wire frame still
  pays the one unavoidable header-prepend `memcpy` (`encode_unified_video_frame`
  takes `&EncodedPacket` and uses `extend_from_slice`, not `Vec::append`). The
  `*_allocates_exactly_once` test guards the single per-frame allocation — don't
  revert to broadcasting the packet by value.
- **Idempotent input moves are dropped on a full channel, never awaited.**
  `dispatch_signaling_message` `try_send`s pointer/touch *moves* (each carries an
  absolute position, so a stale one is harmless once the next arrives); a move
  flood must not `.await`-fill the bounded channel and head-of-line-block a
  click/keystroke on the single WS receive loop. down/up/cancel, **wheel** (its
  deltas accumulate — dropping loses scroll), and keys stay reliable (`.await`).
  Don't make moves reliable; don't make wheel/buttons droppable.
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
- **Encoder writes are coalesced — that throttle lives in the `Controller`, not
  the algorithm, on purpose.** Each `ChangeBitrate` rebuilds the encoder + emits
  an IDR (see Encoder), and AIMD proposes a new target every tick, so the
  `Controller` only actuates a *growth* step once the target has pulled
  `APPLY_THRESHOLD_FRACTION` (15%) past the last-applied rate (`should_actuate` +
  `last_applied_bitrate`); congestion *cuts* and the `max_bitrate` ceiling
  always actuate immediately. This is deliberately not in `BitrateAlgorithm`:
  the algorithm decides the ideal *target*, but "how often is it worth paying an
  encoder rebuild to actuate it" is an encoder-cost actuation policy. The pure
  target still moves every tick (its tests are unchanged).

### Client (`web/`)
- **`ensureCanvasSize` re-checks every frame**, deliberately not gated by a
  one-shot "did we just resize" flag. A resize request and the video stream are
  independent directions on the `/client` socket, so the first frame can arrive
  at the old resolution before the resize lands; a latch would stretch every
  later frame forever.
- **A dead WebCodecs decoder is permanent** — on error it goes `closed` and
  `reset()/configure()` throw. Recovery = a brand-new `VideoDecoder` instance
  then resync from a keyframe (`recoverDecoder`).
- **The client clamps resize to a hardcoded 3840×2160** because the server
  doesn't advertise (or enforce) `max_resolution` over the wire. Both halves
  are sub-16-aligned (`/16`) for H.264.
- **Reconnect is manual, not automatic** (changed from the old backoff
  auto-reconnect; `backoff.ts` is gone). The server allows **one client at a
  time** and kicks the previous connection when a new one arrives, so an
  auto-reconnecting client would fight a second tab forever. `ClientChannel`
  stays `closed` on a dropped/kicked socket and only `reconnect()`s on the next
  canvas interaction (Stage wires pointerdown/touchstart/keydown); `close()`
  (intentional teardown) never reconnects. `onClosed` only fires on an
  unexpected close, so the overlay can prompt the user.
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
- **The receive buffer is viewed, not sliced.** `onUnifiedFrame` hands
  `(buf, byteOffset, byteLength)` to `parseVideoFramePayload`/
  `parseAudioFramePayload`/`onControlPayload` so the payload is a *view* over the
  WebSocket message's `ArrayBuffer` (fresh per message), not a `buf.slice` copy
  on the main thread. The video view's buffer is then **transferred** to the
  decode worker (zero-copy), so `buf` must not be touched after dispatch.
  Clipboard images (rare) keep the slice. Don't reintroduce a per-frame slice.
- **`VideoFrame.close()` runs in a `finally`.** An un-closed frame holds a slot
  in the decoder's output pool and stalls decoding once the pool drains, so
  `handleFrame` closes it even if `renderer.draw()` throws.
- **The WebGL renderer updates the texture in place.** `texImage2D(frame)`
  *reallocates* the texture's backing store every call; `draw()` only
  `texImage2D`s when the frame's coded size changes and `texSubImage2D`s
  otherwise. Both derive their size from the same `VideoFrame`, so a same-size
  `texSubImage2D` always fits what `texImage2D` allocated (no coded-vs-display
  assumption). `texW`/`texH` reset on context loss (the texture is recreated).

### Clipboard bridge (`src/clipboard.rs` + `web/src/lib/clipboard.ts`)

Took many commits across both sides; the subtleties:

- **Why a separate data-control client at all.** Sessions usually run a *nested*
  compositor (see below), so our compositor's only Wayland client is that nest
  — the apps' clipboard lives in the nest and never reaches our own
  `wl_data_device`. The bridge instead connects, as a **`data-control` client**,
  to the *nested* compositor's socket (focus-independent clipboard access, what
  clipboard managers use). No daemon: it's an in-process thread.
- **ext vs wlr.** Prefer `ext-data-control-v1`, fall back to legacy
  `zwlr-data-control`. labwc 0.8/wlroots 0.18 only has wlr; KDE 6 / GNOME ≥ 49 /
  recent wlroots have ext. **cage exposes neither → no clipboard under cage.**
- **Socket discovery (`src/session.rs`)** must test **connectability, not just
  filename**: the nest reuses the conventional `wayland-0`, and a stale
  `wayland-0` *file* from a prior run would defeat a name-only diff — but it
  isn't connectable. Match `wayland-<digits>` only (ours is `wayland-wws-*`,
  per-app proxies are `wayland-proxy-*`), exclude what was already live before
  spawn.
- **Event-driven, no polling.** The device's `selection` event drives
  remote→device. It runs on a calloop loop (`calloop-wayland-source`) with a
  channel for device→remote, so it stays single-threaded.
- **The `owning` flag prevents a loop *and* a self-deadlock.** After we
  `set_selection` (device→remote) the compositor echoes a `selection` event for
  our own source; reading it would loop, and worse, deadlock — we'd be both the
  pipe's writer (source `send`) and its blocking reader. So skip reads while we
  own the selection; clear ownership on the source's `cancelled` event (someone
  else took the selection) → the next `selection` is a real remote change.
- Dedup by **raw bytes** (not MIME label); 8 MiB cap both directions.
- **Browser permission traps (the part that caused regressions):**
  - Never read the device clipboard on a **mouse/keyboard** gesture — Firefox/
    Safari pop a "Paste" affordance that hijacks the click. device→remote
    triggers are: the **`paste` event** (Ctrl+V; `clipboardData`, no prompt), a
    **touch** gesture (mobile, armed once per focus), and a proactive read on
    focus **only when `clipboard-read` is already granted** (Chrome; gated via
    `navigator.permissions` so Firefox/Safari don't pop menus). The proactive
    read is what makes the remote's own right-click→Paste see the device
    clipboard, not just browser Ctrl+V.
  - **Read before flushing a deferred write.** A `writeText`/`write` consumes
    the gesture's transient activation, so a read *after* it throws
    `NotAllowedError` — this silently broke mobile paste until reordered.
- **Drag-and-drop and text-input-v3 "auto-show keyboard" are NOT bridgeable**
  through the nested model (data-control is clipboard-only; DnD/text-input need
  focus/grabs the nest won't forward). They'd require running apps as direct
  clients of our compositor.

### On-screen keyboard (`web/src/lib/softKeyboard.ts` + `OnScreenKeyboard.svelte`)

- **Mobile soft keyboards don't emit usable `KeyboardEvent.code`** (Android
  GBoard → `keyCode 229`/"Unidentified"). So a hidden `<textarea>` captures
  input and we **diff its `value`** on `input` to derive keystrokes. Do *not*
  transcribe `beforeinput` snapshots — IME composition fires *cumulative*
  snapshots and would duplicate ("hello"→h, he, hel…). Diff output is mapped to
  US-layout `code`+shift and reuses the normal key pipeline (works in every app
  incl. XWayland; no Unicode beyond the US layout).
- **The floating button must be non-focusable** (`<div role="button">`, plus
  `preventDefault` on pointerdown). A focusable control grabs focus on tap and
  blurs the hidden field, closing the keyboard the instant it opens.
- **The hidden field must NOT be `aria-hidden`** — Chrome blocks and *removes*
  focus from an aria-hidden element, so the keyboard opens then closes.
- **`pointercancel` ≠ tap.** The browser fires it when it claims the gesture as
  a scroll; treating it like `pointerup` made scrolling over the button pop the
  keyboard.

### Sessions & nested compositors (`src/session.rs`)

- The `-- <cmd>` session can be a single app *or* a nested compositor hosting a
  whole desktop (`-- labwc`, `-- sway`, `-- cage -- firefox`), set
  `WLR_BACKENDS=wayland` so it nests into our headless display. The nest is a
  **single opaque surface** to us — we always inject input into that one
  surface; what it does internally is downstream.
- **A misbehaving in-app dialog (e.g. Firefox's file chooser) can freeze input
  inside the nest and it is NOT our bug.** Verified: our compositor still
  delivers every touch (`windows=1, surface=true`) during the freeze; the
  in-app GTK modal grabs input. The fix is environmental — install
  `xdg-desktop-portal` + a backend so the chooser is a separate portal window
  rather than a grabbing modal.

### Mobile touch (`src/input/touch.rs`, 6 commits to get right)

- **1:1 coordinate mapping.** `surface_at` passes compositor coords straight
  through as surface-local (both SW and GL renderers clip 1:1, never scale). An
  old bbox-derived scale factor delivered touches up to ~3.3× off on the first
  mobile page load.
- **No phantom touches across sessions.** On `/client` disconnect the server
  sends `TouchEvent::Cancel` so the next session doesn't inherit active
  contacts; `touchcancel` clears **all** active touches (the browser's
  changedTouches list can be incomplete when contacts went off-screen).
- The client normalizes touch coords against **`visualViewport`** (accounts for
  mobile browser chrome / soft keyboard), not `innerWidth/Height`.

## Build & test

- `cargo build` runs `build.rs` → `npm ci && npm run build` in `web/`, embeds
  `web/dist/`. It's a no-op when `web/**` is unchanged (mtime check, not just
  Cargo's per-fingerprint tracking). Missing `npm` falls back to a stale
  `web/dist/` with a warning, or panics if none exists.
- For frontend work: `cargo run` (backend :8080) + `cd web && npm run dev`
  (Vite proxies `/client`, HMR against the real compositor).
- `wayland-test-client/` is a minimal Wayland client used by the integration
  tests (and handy for manual checks against `--display-name`).

## Conventions

- Keep the **wire protocol** in lockstep across both sides; it's the contract
  between binary and bundle: `SignalingMessage`/`ServerMessage` in
  `src/server.rs` + the binary framing/`MSG_*` constants and clipboard-image
  payload in `src/proto.rs`, all mirrored in `web/src/lib/protocol.ts`.
- **Logging honors `RUST_LOG`** via `EnvFilter` (`src/main.rs`; it was once
  hardcoded to `INFO`, which silently ignored `RUST_LOG` — don't reintroduce
  that). `info` is lifecycle-only; per-event/diagnostic logs (window map/unmap,
  cursor, touch/pointer, clipboard) are `debug`. Trace input/clipboard with
  `RUST_LOG=info,waylandwebstream=debug`.
- Prefer adding CLI knobs in `src/config.rs` (clap) over hardcoding.
- This project links FFmpeg/x264 and is **AGPL-3.0**; the resulting binary may
  carry GPL terms depending on the FFmpeg build.
