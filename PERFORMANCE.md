# Performance & Smoothness Checklist

Investigation notes and a prioritized action list for closing the smoothness/latency
gap vs. [Selkies](https://github.com/selkies-project/selkies) (both software-only
encode, constant-quality, localhost).

The pipeline, one frame:

```
client SHM buffer
  └─(copy #1: per-pixel scaling loop)→ render_buffer        [compositor/state.rs render()]
      └─(move via mpsc)→ RawFrame
          └─(copy #2: copy_from_slice)→ ffmpeg BGRA frame   [encoder/mod.rs encode_frame()]
              └─(copy #3: swscale BGRA→YUV420P)→ YUV frame
                  └─ x264 encode
                      └─(copy #4: .to_vec())→ EncodedPacket
                          └─(move→Bytes)→ TrackLocalStaticSample.write_sample
```

Channel hops are moves, not copies. The cost is concentrated in `render()` and the
encoder's per-frame frame setup — plus two non-copy issues (frame pacing and the
client jitter buffer) that are the most likely reasons Selkies looks smoother.

---

## Tier 1 — Smoothness & latency (do these first)

These are small, low-risk, and target the actual complaint more directly than copy
reductions. Land 1–3 together so you can A/B against Selkies.

- [x] **Fix frame pacing** — `main.rs:263-343`. Capture is gated on
  `loop_start.duration_since(last_frame) >= frame_interval`, then sets
  `last_frame = loop_start`. Problems:
  - Timing is only checked once per loop iteration, and the iteration is gated by
    `event_loop.dispatch(16ms)` + render cost. At the default **60fps (16.67ms)** the
    deadline is routinely missed → frames land at ~16/32ms → judder, effective FPS
    drifts toward 30.
  - `last_frame = loop_start` snaps to wake time, so interval error accumulates
    instead of self-correcting. Frames are never evenly spaced.
  - **Try:** drive capture from a calloop `Timer` source at `frame_interval`, and
    accumulate `last_frame += frame_interval` instead of snapping to wake time.
  - This is the single highest-leverage smoothness fix.
  - **Done:** replaced `last_frame` with a self-correcting `next_frame` deadline
    (`next_frame += frame_interval`, resyncing to `now` only after a stall) and
    made the `event_loop.dispatch` timeout track time-remaining-until-deadline
    (capped at 16ms) instead of always waiting a fixed 16ms. Skipped the literal
    calloop `Timer` source — the loop already calls `dispatch` manually each
    iteration with an explicit timeout, so a `Timer` source would duplicate that
    without changing behavior; the dynamic timeout achieves the same effect with
    a smaller diff.

- [x] **Stop zeroing the client jitter buffer** — `client.html:90-91`
  (`jitterBufferTarget = 0`, `playoutDelayHint = 0`). With capture jitter (above),
  a zero playout buffer turns every timing irregularity into a stall/drop. Selkies
  does *not* zero this. **Try:** default, or a small target (~50–100ms), then measure.
  Counterintuitively likely to *lower* perceived latency by removing stutter.
  **Done:** set both `jitterBufferTarget` and `playoutDelayHint` to `0.05` (50ms)
  instead of `0`. Picked the low end of the suggested range to keep added latency
  small while still giving the decoder room to absorb timing jitter. Should be
  A/B'd against Selkies per the measurement task below.

- [x] **Use capture-based sample timestamps** — `session.rs:255-261`.
  `Sample { timestamp: SystemTime::now(), duration: frame_duration }` makes RTP
  timestamps reflect *send* time (jittery), not *capture* time. `packet.capture_time`
  is already carried end-to-end — derive a monotonic capture-based timestamp so playout
  cadence matches capture cadence. Interacts badly with the zero jitter buffer above.
  **Done:** added a `capture_epoch: Mutex<Option<(Instant, SystemTime)>>` to
  `Session`, set from the first packet's `capture_time`/`SystemTime::now()` pair.
  Every later packet's `Sample.timestamp` is now `base_systime +
  (packet.capture_time - base_instant)`, so RTP timestamps track capture cadence
  instead of send-time jitter (encoder/channel scheduling delay no longer shows
  up as timestamp noise).

- [ ] **Measure before/after.** Use the existing latency reporting + `getStats()`
  output in `client.html`. Watch jitterBufferDelay, framesDecoded cadence, and the
  per-stage server report. A/B each Tier-1 change against Selkies on the same content.

## Tier 2 — Copy & allocation reduction (biggest CPU wins)

- [x] **Reuse the render framebuffer** — `state.rs:172-182`. `vec![0u8; w*h*4]`
  (~8MB at 1080p) is allocated **every frame** and cleared pixel-by-pixel. Keep a
  persistent buffer on the state struct; clear with `fill(0)`, or skip the clear for
  the region a fullscreen window fully overwrites.
  **Done:** `render()` now takes `Option<Vec<u8>>` and reuses/resizes it instead of
  always allocating, and clears with `fill(0)` instead of a per-pixel store loop
  (alpha is never read downstream, so a plain memset is safe). The buffer still has
  to cross the render→encoder thread boundary by move (it's sent in `RawFrame`), so
  true reuse needed a way to get it back: `encoder/mod.rs` now has a
  `std::sync::mpsc::Sender<Vec<u8>>`/`Receiver` pair (`BufferReturnReceiver`) —
  the encoder thread sends `raw_frame.data` back immediately after
  `encode_frame` (which only borrows it) returns, and `main.rs` drains that
  receiver into a small `Vec<Vec<u8>>` pool each frame, popping one to pass into
  `state.render()`. Didn't implement the "skip clear for fullscreen window" path —
  verifying a window's render fully covers the buffer (position, partial commits,
  multiple windows) is easy to get subtly wrong, and `fill(0)` already turns the
  per-pixel loop into a single memset, which is most of the win.

- [ ] **Add fast paths to the scaling copy** — `state.rs:238-254`. This is likely the
  largest single CPU cost. Per output pixel it does two `u64` divisions, two bounds
  checks, and a 4-byte copy (~125M iterations/sec at 1080p60), on the compositor thread.
  Windows are configured fullscreen, so in steady state **buffer size == output size** —
  yet the general scaling path always runs. Add:
  - `buffer_w==target_w && buffer_h==target_h && stride==width*4` → one whole-buffer
    `copy_from_slice` (single memcpy).
  - vertical-only scaling → per-row `copy_from_slice`.
  - general case → precompute the `src_x` lookup table once per row instead of dividing
    per pixel.

- [ ] **Reuse encoder frames & drop the BGRA intermediate** — `encoder/mod.rs:357-374`.
  - Both `frame::Video` allocations are fixed-size — allocate once, reuse across calls,
    reset on resize.
  - Feed swscale directly from the render buffer's pointer+stride to eliminate copy #2
    (`data_mut(0).copy_from_slice(&raw_frame.data)`) entirely.
  - That `copy_from_slice` also assumes `linesize == width*4`; works at 16-aligned
    widths but is fragile with stride padding — copy row-by-row respecting `linesize`,
    or remove the copy as above.

## Tier 3 — Further efficiency

- [ ] **Damage tracking.** Every frame is fully re-rendered + color-converted even on a
  static screen. x264 emits tiny P-frames so *encode* stays cheap, but the render copy
  and swscale run unconditionally. Add a dirty flag to skip render+encode when the
  buffer is unchanged (modulo keyframe cadence).

- [ ] **swscale threading / flags** — `encoder/mod.rs:335-347`. `FAST_BILINEAR` is
  irrelevant since src dims == dst dims (pure colorspace convert). swscale is
  single-threaded; consider thread count. Lower priority — the render loop dominates.

- [ ] **Encoder queue drops** — `main.rs:334` `frame_sender.try_send` silently drops
  frames when the encoder lags (channel cap 4). Fine as backpressure, but worth logging/
  counting so dropped frames don't masquerade as a pacing problem.

## Tier 4 — Cleanup (low risk, reduces confusion)

- [ ] **Remove dead code:** `encoder/frame.rs` `FrameCapture` is entirely unused
  (`main.rs` does its own pacing); `state.rs:285 get_framebuffer()` returns a throwaway
  zero buffer and is never called.

- [ ] **Replace `static mut FRAME_COUNTER`** — `state.rs:187`. Unsynchronized
  `static mut` is UB-adjacent; use an instance field or `AtomicU32`.

---

## Reference: how Selkies differs

- GStreamer pipeline clock paces buffers evenly (vs. the hand-rolled, drifting loop here).
- Keeps a small default jitter buffer (vs. zeroing it here).
- Damage-aware source + (often) multithreaded `videoconvert`.

The two highest-leverage fixes are **(Tier 1) fix pacing** and **(Tier 1) stop zeroing
the jitter buffer**. The copy reductions (Tier 2) lower CPU and indirectly help pacing
by making the render loop cheaper.
