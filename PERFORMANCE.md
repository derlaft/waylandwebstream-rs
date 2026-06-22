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

- [x] **Add fast paths to the scaling copy** — `state.rs:238-254`. This is likely the
  largest single CPU cost. Per output pixel it does two `u64` divisions, two bounds
  checks, and a 4-byte copy (~125M iterations/sec at 1080p60), on the compositor thread.
  Windows are configured fullscreen, so in steady state **buffer size == output size** —
  yet the general scaling path always ran.
  **Done:** added a `buffer_width == target_width && buffer_height == target_height`
  fast path that copies row-by-row with `copy_from_slice` (respecting `buffer_stride`,
  so no assumption about padding), replacing the per-pixel divide+copy loop for the
  steady-state case. Deliberately did **not** add the vertical-only or general-case
  (lookup-table) fast paths from the original plan — actual scaling (buffer size !=
  target size) only happens for the frame or two a client lags a viewport resize by,
  so the old per-pixel loop stays as the fallback for that transient path; optimizing
  it further isn't worth the added code for something that's never the steady state.
  User note: this made the first easily visible positive improvement. 

- [x] **Reuse encoder frames & drop the BGRA intermediate** — `encoder/mod.rs:357-374`.
  - Both `frame::Video` allocations are fixed-size — allocate once, reuse across calls,
    reset on resize.
  - Feed swscale directly from the render buffer's pointer+stride to eliminate copy #2
    (`data_mut(0).copy_from_slice(&raw_frame.data)`) entirely.
  - That `copy_from_slice` also assumes `linesize == width*4`; works at 16-aligned
    widths but is fragile with stride padding — copy row-by-row respecting `linesize`,
    or remove the copy as above.
  **Done:** `input_frame` (BGRA) and `yuv_frame` (YUV420P) are now created once in
  `encoder_thread` and reused across calls, recreated only on resize/reinit (same
  place the encoder/scaler already get recreated). `input_frame` is never allocated
  via `alloc()` — `encode_frame` points its `data[0]`/`linesize[0]` straight at each
  `RawFrame`'s buffer (`linesize = width*4`, which matches how `render()` packs
  `render_buffer` with no row padding, so this isn't the fragile assumption the
  original `copy_from_slice` made), removing copy #2 entirely. Verified `Video::empty()`
  + manual `set_format`/`set_width`/`set_height` never calls `av_frame_get_buffer`
  (checked `ffmpeg-next` 8.1.0 source), so `input_frame` never owns a buffer and the
  pointer-aliasing is safe — `raw_frame` outlives the `sws_scale` call that reads it.
  `yuv_frame` *is* a real owned/refcounted buffer (`Video::new()` → `alloc()`), so
  reusing it relies on the encoder having zero frame delay (tune=zerolatency,
  bframes=0, no lookahead) so that draining `receive_packet()` to EAGAIN each call
  guarantees libx264 is done reading it before the next `scaler.run()` overwrites it;
  documented this invariant in `encoder/mod.rs` since it'd silently break if B-frames/
  lookahead were ever turned on. Confirmed via `cargo test -- --test-threads=1`
  (the suite isn't parallel-safe — two tests share the `wayland-test-0` socket name —
  that's pre-existing and unrelated) that the full pipeline, including a mid-stream
  resize that reinitializes the encoder/scaler/frames, still passes.

## Tier 3 — Further efficiency

- [x] **Damage tracking.** Every frame is fully re-rendered + color-converted even on a
  static screen. x264 emits tiny P-frames so *encode* stays cheap, but the render copy
  and swscale run unconditionally. Add a dirty flag to skip render+encode when the
  buffer is unchanged (modulo keyframe cadence).
  **Done:** added a `dirty: bool` to `WaylandWebStreamState` (`state.rs`), set on any
  surface commit, window map/unmap, or output resize, and consumed via
  `take_dirty()`. `main.rs`'s capture loop now only calls `state.render()` (and feeds
  the encoder) when `take_dirty()` is true, a new WebRTC session just connected, or
  `keyframe_interval` ticks have passed since the last actual render — the latter is
  a safety net so a fresh keyframe still goes out periodically on a static screen
  (decoder resync after loss) and so a late joiner isn't stuck waiting indefinitely.
  The "new session" case needed its own plumbing: `SessionManager` already requests
  `EncoderControl::ForceKeyframe` on a new offer, but that only takes effect on the
  *next* frame the encoder receives — with damage tracking, an idle screen might not
  produce one for a while. Added a `force_render: Arc<AtomicBool>` threaded from
  `main.rs` into `SessionManager`, set alongside the existing `ForceKeyframe` send, so
  the capture loop renders immediately for a newly connected client instead of
  leaving it with no video until the screen changes or the periodic safety net fires.
  Did not attempt buffer-content comparison (e.g. hashing) to detect no-op commits —
  that cost would likely exceed the render it's meant to avoid skipping; a commit is
  conservatively always treated as damage.

- [x] **swscale threading / flags** — `encoder/mod.rs:335-347`. `FAST_BILINEAR` is
  irrelevant since src dims == dst dims (pure colorspace convert). swscale is
  single-threaded; consider thread count. Lower priority — the render loop dominates.
  **Done:** swapped `FAST_BILINEAR` for `POINT` in `create_scaler` -- functionally a
  no-op (confirmed in the `ffmpeg-next` source: src/dst dims are always identical
  here, since the encoder is reinitialized to match on every resize, so swscale's
  `sws_init_context` takes its dedicated "unscaled" SIMD converter path for a pure
  colorspace conversion, selected purely by format pair and bypassing the resampling
  filter the flag controls entirely) -- but `POINT` states the actual intent (no
  interpolation) instead of naming a filter that's never built. Did **not** pursue
  swscale thread count: the safe `ffmpeg-next` API (`Context::get` → `sws_getContext`)
  doesn't expose it -- it requires the raw `sws_alloc_context`/`av_opt_set_int`/
  `sws_init_context` sequence via `ffmpeg_next::ffi` instead of a single call, more
  unsafe surface for a function that's already on the SIMD unscaled-converter path.
  Given the doc's own conclusion that the render loop dominates, and that swscale's
  threading model parallelizes by row-slices of a resize filter that doesn't run
  here, the expected win didn't justify the added unsafe code and an extra failure
  mode (thread-pool setup) on this path.

- [x] **Encoder queue drops** — `main.rs:334` `frame_sender.try_send` silently drops
  frames when the encoder lags (channel cap 4). Fine as backpressure, but worth logging/
  counting so dropped frames don't masquerade as a pacing problem.
  **Done:** added a `dropped_frames: u64` counter in the capture loop, incremented on
  `try_send` failure. Logs a `warn!` on the first drop and every 30th after that
  (matching the existing "log every 30 frames" cadence used elsewhere in the codebase)
  rather than every single drop, since under sustained encoder lag this could fire up
  to once per tick. A dropped frame also now counts toward `ticks_since_render`'s
  staleness tracking (added for damage tracking, above) -- the encoder didn't actually
  get a fresh frame that tick, so it should still count toward forcing the next
  periodic render.

## Tier 4 — Cleanup (low risk, reduces confusion)

- [x] **Remove dead code:** `encoder/frame.rs` `FrameCapture` is entirely unused
  (`main.rs` does its own pacing); `state.rs:285 get_framebuffer()` returns a throwaway
  zero buffer and is never called.
  **Done:** deleted `encoder/frame.rs` and its `pub mod frame;` declaration, and
  removed `get_framebuffer()`. Confirmed both were unreferenced anywhere outside
  their own definitions before removing.

- [x] **Replace `static mut FRAME_COUNTER`** — `state.rs:187`. Unsynchronized
  `static mut` is UB-adjacent; use an instance field or `AtomicU32`.
  **Done:** replaced with a `frame_counter: u32` field on `WaylandWebStreamState`,
  incremented (with `wrapping_add`) at the top of `render()`, which already has
  `&mut self`. The nested buffer-contents closure that also read the counter now
  reads a local `Copy` snapshot taken before the loop instead of the static, so no
  `unsafe` is needed anywhere in `render()` for this anymore.

---

## Reference: how Selkies differs

- GStreamer pipeline clock paces buffers evenly (vs. the hand-rolled, drifting loop here).
- Keeps a small default jitter buffer (vs. zeroing it here).
- Damage-aware source + (often) multithreaded `videoconvert`.

The two highest-leverage fixes are **(Tier 1) fix pacing** and **(Tier 1) stop zeroing
the jitter buffer**. The copy reductions (Tier 2) lower CPU and indirectly help pacing
by making the render loop cheaper.
