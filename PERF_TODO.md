# Pipeline Performance & Latency — Action Checklist

Glass-to-glass review of the full video-delivery pipeline (browser → backend
→ capture) and the input return path. Ordered by leverage; check off as done.

## 1. Wake the compositor loop on damage + input  `[HIGH]`
The synchronous compositor loop (`src/main.rs`) is paced by
`event_loop.dispatch(min(time_to_next_frame, 16ms))` and only acts on a fixed
frame grid. This adds ~½ frame of latency **twice**:
- **Video:** damage landing just after a tick waits until the next grid point
  before capture (`main.rs:654-724`). The loop wakes on Wayland traffic but
  re-checks `now >= next_frame` and never renders early on damage.
- **Input:** input mpsc channels are drained with `try_recv()` once per loop
  iteration with no wake on send (`main.rs:617-633`).

**Fix:** make the tokio→sync handoff wake the loop — register input channels as
`calloop` sources or ping a `calloop::ping::Ping`/`LoopSignal` on send; render
immediately when `take_dirty()` is true and the min inter-frame interval has
elapsed (cap the *max* rate, don't quantize to the grid). Removes ~8ms median
from both video and input.

- [x] **Done.** `src/main.rs`: added a `calloop::ping::Ping` (`make_ping`) the
  async input/resize handlers fire via `SignalingState::wake_input_loop`
  (`src/server.rs`) to break `dispatch()` out immediately; input drain moved to
  after `dispatch_clients` so injected events flush in the same iteration. The
  render gate now also fires on `state.is_dirty()` / new-client once one frame
  interval has elapsed since the last capture (`last_capture`), capping
  throughput at the framerate. Verified on s8: 56 unit tests + render_pixels
  pass, clean startup, live capture streams firefox correctly.

## 2. Encoder: in-place bitrate change (no IDR / no rebuild)  `[HIGH]`
`encoder/mod.rs:362-385`, `vaapi.rs:227-250`: every bitrate change tears down
and rebuilds the encoder and resets `frame_count=0`, forcing an unscheduled IDR
~once per second (AIMD climbs every tick). Each IDR is a 5–15× bandwidth spike
that fights the controller.
**Fix:** change bitrate in place (libx264 `x264_encoder_reconfig` / don't reset
the frame counter; VAAPI live RC update); never force an IDR on a rate change.

- [x] **Done — via coalescing, not in-place reconfig.** Finding: libavcodec
  (ffmpeg 7.1 / ffmpeg-next 8.1) exposes **no** runtime bitrate reconfig for
  x264 *or* h264_vaapi — the IDR is intrinsic to any encoder rebuild, so
  "in-place, no IDR" isn't reachable through the binding. Real fix: make
  rebuilds rare. `src/adaptive_bitrate.rs` now decouples the finely-tracked
  AIMD target from the rate actually pushed to the encoder: a growth step only
  actuates a `ChangeBitrate` once it clears `APPLY_THRESHOLD_FRACTION` (15%) of
  the last-applied rate (`should_actuate` + `last_applied_bitrate` in the
  controller); congestion cuts and the `max_bitrate` ceiling always actuate
  immediately. Collapses the ~1 Hz CA-growth IDR train to rarer-than-GOP. Also
  corrected the misleading `frame_count = 0 // force IDR` comments in
  `encoder/mod.rs` + `vaapi.rs` (the rebuild emits the IDR, not the counter).
  Verified on s8: 60 bin + 24 lib (4 new `should_actuate`) + 8 algorithm tests
  pass; live ramp reaches max (12 Mbps) exactly, streams firefox; algorithm
  untouched so its tests are unchanged.

## 3. Encoder: drain-to-newest in the encode loop  `[HIGH]`
`encoder/mod.rs:448-487` encodes every queued frame in order. On a hiccup, up
to 4 stale frames each cost ~33ms and back-pressure capture.
**Fix:** `try_recv`-drain to the freshest frame before encoding (return skipped
buffers), as the resize path already does.

- [x] **Done.** `src/encoder/mod.rs`: added `skip_to_newest_frame`, called right
  after `blocking_recv` — drains any frames queued behind the one just pulled,
  keeping only the newest and returning each skipped `Cpu` buffer via
  `buffer_return_tx` for reuse (mirrors the resize-drain path). Self-regulating:
  a no-op when the encoder is keeping up (only one frame queued), skips ahead
  only when it fell behind. Extracted as a pure helper so it's deterministically
  unit-tested (no encoder-thread race). Verified on s8: 62 bin + 26 lib (2 new
  drain tests) pass, streams firefox cleanly.

## 4. Kill the three whole-frame copies on the hot path  `[MEDIUM]`
- [ ] Capture handoff (`compositor/state.rs:697-703`): full ~8MB memcpy to the
      encoder even for tiny damage. Pass damage rect / copy only damaged rows.
- [ ] Broadcast clone (`server.rs:476`): `tokio::broadcast` deep-copies the
      payload per recv. Single-client design → broadcast `Arc<EncodedPacket>`.
- [ ] Client receive slice (`client.ts:207`): `buf.slice(8, totalLen)` memcpy's
      the whole frame on the main thread. View directly over the received
      ArrayBuffer instead (0 copies + 1 transfer).

## 5. Decoder: `prefer-hardware` probe  `[MEDIUM]`
`protocol.ts:255-258` uses default `no-preference`; Firefox intermittently
picks SW (~30–70ms decodes vs <5ms HW). Add an `isConfigSupported`-gated
`prefer-hardware` probe once at setup (see AGENTS.md for the safe pattern /
infinite-loop trap).

- [ ] Done

## 6. Encoder: intra-refresh + tighter VBV + proportional AIMD recovery  `[MEDIUM]`
`encoder/mod.rs:652-677`: 2s VBV + periodic 2s IDRs are a latency floor;
`adaptive_bitrate.rs:111-124` recovers at a flat +150kbps/s (~10s for one cut).
**Fix:** `--intra-refresh` + longer GOP (rely on client keyframe-requests),
tighten VBV toward ~500ms, scale AIMD increase with current rate. Do after #2.

- [ ] Done

## 7. Input: coalesce moves + non-blocking server send  `[MEDIUM]`
`input.ts:66-69,111-114` sends every pointer/touch move individually; bounded
channels fill and the server `.await`-send (`server.rs:769-780`) head-of-line
blocks clicks/keystrokes. Coalesce moves per rAF (always flush final position);
`try_send`/drop-oldest for moves server-side, keep down/up/key reliable.

- [ ] Done

## 8. GL compositor: drop redundant fence / async readback  `[LOW]`
`compositor/gl.rs:226-267` is fully serial (render → sync.wait → blocking
readback). Drop the unconditional fence on the Cpu arm; async PBO readback for
real overlap. Lower priority (zero-copy VAAPI pairing sidesteps it).

- [ ] Done

## 9. Render: `texSubImage2D` at high res  `[LOW]`
`glRenderer.ts:181` reallocates texture storage per frame (~1–4ms GPU at 4K).
Switch to `texSubImage2D` once dims are stable. Negligible at 1080p.

- [ ] Done

## 10. Misc hardening  `[LOW]`
- [ ] try/finally around `renderer.draw(frame)` so a throw can't leak a
      VideoFrame (`stream.ts:234`).
- [ ] Debounce mobile `visualViewport` CSS-size writes (`viewport.ts:120-146`).
- [ ] Track damage as a small rect set, not a single merged bbox
      (`compositor/state.rs:470-475`) — once #4 handoff is damage-proportional.

---

### Already well-tuned (do not regress)
`TCP_NODELAY` on; single-alloc wire framing (alloc-count test); broadcast cap 3
+ Lagged skip-to-live; off-main-thread decode/render in a worker; zero-copy
H.264 transfer + zero-copy WebGL `texImage2D(VideoFrame)`; immediate non-rAF
present + prompt `frame.close()` + `optimizeForLatency`; x264
ultrafast/zerolatency/bframes=0 with sliced threading + direct `data[0]`
aliasing + buffer recycling; damage-gated idle skip.
