# Performance 2 — WebSocket + WebCodecs delivery, then damage-gated encoding

A development plan for closing the remaining smoothness/latency gap vs.
[Selkies](https://github.com/selkies-project/selkies), based on a direct study of
how Selkies actually delivers video.

## Background: what we learned from Selkies

Selkies' default "works so well" path is **not** WebRTC. It is:

```
pixelflux capture/encode  ──WebSocket (binary)──>  WebCodecs VideoDecoder  ──>  <canvas>
```

Key findings from reading the Selkies source and probing a live session:

- The selected `x264enc` mode is **fullframe** (`h264_fullframe = True`): the whole
  frame is one H.264 unit, decoded by a single `VideoDecoder`, blitted once per
  update. Confirmed live via a `drawImage` hook — every video blit was the full
  `2880x1060` frame at `y=0`, never partial bands.
- `x264enc-striped` (horizontal Y-bands, one decoder per band) is a **separate**
  mode. It buys bandwidth on mostly-static screens but is mobile-hostile (Android
  MediaCodec runs out of concurrent decoder instances; high-chroma profiles aren't
  hardware-decodable). **We are deliberately not pursuing stripes.**
- The bulk of Selkies' advantage over this project comes from the **transport and
  decode path** (WebSocket + WebCodecs, near-zero jitter buffer, immediate paint),
  not from striping.

Our current pipeline is the opposite at every stage: full frame encoded **and sent
every cadence tick**, over **WebRTC/RTP**, decoded by the browser's **native
WebRTC jitter buffer** into a `<video>` element (`src/web/client.html`,
`src/webrtc/session.rs`). The native jitter buffer is exactly what
`client.html` fights with `jitterBufferTarget = 0.05` / `playoutDelayHint`.

## Strategy

Two milestones, in order. The first is the big perceived win and is self-contained.
The second is a bandwidth/CPU optimization that does **not** require stripes.

```
Today:   render → x264 (fullframe) → RTP → WebRTC → native <video> + jitter buffer
M1:      render → x264 (fullframe) → WebSocket(binary) → WebCodecs VideoDecoder → <canvas>
M2:      ^ same, but render+encode only fire on real damage (idle screen ≈ 0 cost)
```

---

## Milestone 1 — WebSocket + WebCodecs frontend (retire WebRTC)

**Goal:** deliver the existing fullframe H.264 stream over a binary WebSocket and
decode it in the browser with WebCodecs into a `<canvas>`, removing WebRTC, RTP,
ICE/STUN, and the embedded TURN relay from the data path. This is where the latency
win lands — we own the jitter buffer (there isn't one) instead of begging the
native WebRTC stack to shrink it.

### Server tasks

1. **Surface keyframe-ness on encoded packets.** WebCodecs needs each chunk tagged
   `key` or `delta`. `src/encoder/mod.rs` already produces Annex-B H.264 with
   `repeat_headers=1` (SPS/PPS inline on every keyframe — exactly what WebCodecs
   Annex-B mode wants). Add `is_keyframe: bool` to `EncodedPacket`, read from
   ffmpeg's `Packet::is_key()` in `encode_frame()`.

2. **Add a frame id / timestamp to `EncodedPacket`.** A monotonic `u16`/`u32`
   frame id (wraps fine) for ordering, plus the existing `capture_time` for latency
   accounting.

3. **New binary video WebSocket.** Add a `/stream` (or reuse the existing axum
   server in `src/webrtc/signaling.rs`, which already has `ws` support) endpoint
   that, per connected client, forwards `EncodedPacket`s as binary frames. Wire
   format, little ceremony — mirror Selkies' minimalism:

   ```
   byte 0     : frame_type   (0 = delta, 1 = key)
   bytes 1-4  : frame_id     (u32, big-endian)
   bytes 5..  : raw Annex-B H.264 for the whole frame
   ```

   One frame per WebSocket message. No RTP, no packetization, no muxing.

4. **Move input + control onto the same WebSocket (or a second text WS).** Touch,
   pointer, resize, and latency-report messages currently ride the WebRTC data
   channel / signaling JSON (`SignalingState` in `src/webrtc/signaling.rs`). Re-home
   them as text/JSON frames on a control WebSocket. The channels into the compositor
   (`touch_tx`, `mouse_tx`, `resize_tx`, `latency_tx` in `src/main.rs`) stay exactly
   as they are — only the front door changes.

5. **Backpressure.** If a client's socket can't drain, drop frames rather than
   buffer unboundedly (we already drop on a full encoder queue in `src/main.rs`).
   Keep the per-client send queue small (e.g. 2–3 frames) and skip to the newest;
   on a drop, the next keyframe (periodic, every `keyframe_interval`) resyncs.

6. **Retire the WebRTC data path.** Once M1 is proven, delete/disable
   `src/webrtc/session.rs` (RTCPeerConnection, RTP track), the SDP/ICE handling in
   `src/webrtc/signaling.rs`, and `src/webrtc/turn_server.rs` + the TURN bring-up in
   `src/main.rs`. Drop the `webrtc`, `interceptor`, `turn`, `webrtc-util` deps from
   `Cargo.toml`. (Keep them behind a feature flag only if a fallback is wanted; the
   stated direction is "not using WebRTC anymore.")

### Client tasks (`src/web/client.html`)

1. **Replace `<video>` with `<canvas>`** and remove all `RTCPeerConnection`,
   transceiver, ICE, and `jitterBufferTarget` code.

2. **Open the binary WebSocket**, parse the 5-byte header, build an
   `EncodedVideoChunk { type, timestamp, data }`, and feed a single
   `VideoDecoder`. Gate `delta` chunks until the first `key` has been decoded.

3. **Decoder config.** Use the codec string matching the encoder's
   `profile=baseline / 42e01f` → `avc1.42E01F`, `optimizeForLatency: true`, and
   **default `hardwareAcceleration`** (do *not* force `prefer-software`; baseline
   4:2:0 is hardware-decodable on phones — this is the mobile mistake Selkies makes).
   No `description` is needed: the stream is Annex-B with inline SPS/PPS.

4. **Paint loop.** On each decoded `VideoFrame`, `drawImage(frame, 0, 0)` to the
   canvas, then `frame.close()`. No queue/jitter buffer — paint immediately (or at
   most a one-frame `requestAnimationFrame` cadence).

5. **Latency reporting.** Re-home the existing client→server latency report onto the
   control WebSocket. WebCodecs gives us real decode timing
   (`VideoFrame.timestamp`, decode callbacks) — better signal than the
   `inbound-rtp` stats we scrape today.

### Acceptance criteria

- Single browser tab decodes the live desktop via WebCodecs into a canvas; no
  `RTCPeerConnection` anywhere.
- Measured glass-to-glass latency on localhost **≤** the current WebRTC path, with
  the receive-side queue effectively zero (no native jitter buffer).
- Touch/pointer/resize still work over the new control channel.
- Works in Chrome/Chromium desktop **and** Chrome Android (baseline profile is the
  point). Firefox Android remains JPEG-only territory and is explicitly out of scope
  for the H.264 path.

---

## Milestone 2 — Damage-gated encoding (no stripes)

**Goal:** stop spending encode/transmit/decode work on parts of the screen that did
not change, so an idle or barely-moving desktop costs ≈ 0 — *without* splitting the
frame into stripes/tiles.

### Design note: "only changed regions" without stripes

There is a real constraint worth stating up front, because it shapes the milestone:

- A single fullframe H.264 stream is decoded **as a whole frame**. You cannot decode
  "only a sub-rectangle" of it. Decoding isolated regions is precisely what
  stripes/tiles are for — and we've chosen not to do that (mobile decoder limits,
  complexity, and the live test showing fullframe already performs well).
- **But** "only changed regions" is still largely achieved, in two ways that need no
  stripes:
  1. **Frame-level damage gating** — if nothing changed, encode and send *nothing*.
     The decoder simply holds the last frame on the canvas.
  2. **Native H.264 inter-frame coding** — within a P-frame, unchanged macroblocks
     are coded as *skip* blocks costing near-zero bits. So even when we do send a
     frame, the bytes scale with how much actually changed, not with resolution.

So M2 is really: **make damage detection accurate and use it to gate the
render→encode→send pipeline.** The decoder keeps doing full-frame decodes; we just
stop feeding it redundant frames, and each frame we do send is already dominated by
the changed region thanks to the codec.

### Server tasks

1. **Replace the coarse dirty flag with real damage.** Today `CompositorState`
   tracks a single `dirty: bool` (`src/compositor/state.rs`) that `take_dirty()`
   consumes, and it is set on *any* surface commit / map / unmap — so a one-pixel
   change marks the whole screen dirty and forces a full render+encode. Smithay
   surface commits carry buffer **damage regions**; accumulate those into a damage
   accumulator (a union rect, or a small list of rects) per frame instead of a bare
   bool.

2. **Gate the render loop on real damage.** In `src/main.rs`, the loop already only
   renders when `screen_dirty || new_client || stale`. Tighten `screen_dirty` to
   mean "non-empty accumulated damage." Net effect: a truly static screen produces
   **zero** frames between periodic keyframes (the `keyframe_interval` staleness
   timer already guarantees an occasional resync frame), and a screen with a blinking
   cursor produces small P-frames only when the cursor actually toggles.

3. **(Optional, advanced) Feed damage to the encoder as a hint.** x264 can bias
   bits toward a region of interest / skip outside it, but `ffmpeg-next` exposes this
   awkwardly. Treat as a stretch goal — the win from steps 1–2 plus native P-frame
   skip-blocks is most of the benefit. Do not block M2 on this.

4. **(Optional) Avoid the full-buffer clear + full repaint in `render()`.** Today
   `render()` does `render_buffer.fill(0)` and repaints every window each frame
   (`src/compositor/state.rs`). With a damage rect available, only the damaged
   region needs repainting into the reused buffer. This reduces the per-frame CPU in
   the compositor (copy #1 in `PERFORMANCE.md`) even though the encoder still sees a
   full frame. Optional; measure first.

### Client tasks

- **None required.** The client still receives and decodes whole frames; it simply
  receives them less often. This is the payoff of avoiding stripes — the frontend
  from M1 is unchanged.

### Acceptance criteria

- Idle desktop (no input, static content): outbound bandwidth and encoder CPU drop
  to ~keyframe-cadence baseline; `drawImage`-equivalent paints on the client fall to
  near zero between keyframes.
- Cursor-only / small-edit activity: bandwidth scales with the changed area, not the
  resolution.
- No regression in latency or correctness for full-motion content (video playback,
  scrolling) — damage there is large, so behavior approaches today's every-frame path.

---

## Explicitly out of scope

- **Stripes / tiles** (`x264enc-striped`-style per-band encode and per-band
  decoders). Rejected: mobile decoder-instance limits, high-chroma profile issues,
  and the live finding that fullframe already performs well. Per-region *decode* is
  the only thing stripes would add, and we don't need it.
- **High-chroma H.264** (4:2:2 / 4:4:4, the `avc1.7A00…` / `F400…` profiles Selkies
  uses). We stay on baseline 4:2:0 for universal hardware decode, including mobile.
- **Audio.** Not part of this plan.

## Suggested sequencing

1. M1 server steps 1–5 (add WebSocket video path **alongside** the existing WebRTC
   path) so you can A/B latency before deleting anything.
2. M1 client rewrite; verify on desktop Chrome + Android Chrome.
3. M1 step 6: delete WebRTC/RTP/TURN once the WS path wins.
4. M2 steps 1–2 (accurate damage + render gating). Measure idle bandwidth.
5. M2 steps 3–4 only if profiling says the compositor/encoder is still the bottleneck.
