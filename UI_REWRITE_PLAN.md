# UI Rewrite Plan — Plain Svelte client

Status: **planned** (2026-06-23). Implementation not started.

This is an execution checklist for agents. The goal is to replace the single
embedded `src/web/client.html` with a small, modern, **plain Svelte + Vite**
client that fixes the scaling/input bugs and adds a collapsible side panel.

---

## 0. Goals & non-negotiables

- **Mobile low-latency experience first**, desktop second.
- **Minimal dependencies, minimal output, fast.** Plain Svelte compiled to a
  vanilla-JS bundle — no SvelteKit, no router, no SSR, no Node runtime at
  serve time.
- **Single self-contained Rust binary** stays the deploy unit: the compiled
  bundle is embedded into the binary.
- **Do not change the wire protocol** (see §3). The server keeps working with
  zero or near-zero changes; all the hard work is client-side.
- Sharpness over convenience: video maps **1:1 to device pixels** (100% scale),
  never CSS-upscaled/blurred.

## 1. Locked decisions

| Decision | Choice |
|---|---|
| Framework | **Plain Svelte 5 + Vite + TypeScript** (no SvelteKit) |
| Delivery | **Embedded in the Rust binary** (Vite dev server proxies in dev) |
| Scaling | **1:1 device pixels**, canvas fills viewport, small margins where viewport isn't /16-aligned; sharp, never stretched. Resize requests **throttled**. A `scaleFactor` knob (default `1`) is designed in now; a **2× client-side scale button comes later** for hidpi perf. |
| Side panel (build now) | Collapsible noVNC-style panel with **Fullscreen button** (required) + **Connection / latency stats**. Everything else (manual reconnect, force-keyframe, on-screen keyboard) is deferred but the panel must be structured to add them trivially. |

## 2. Target layout

```
web/                         # new Vite + Svelte source (gitignored: node_modules, dist)
  package.json
  vite.config.ts             # build + dev proxy for /ws and /stream
  svelte.config.js
  tsconfig.json
  index.html                 # single entry; <div id="app">
  src/
    main.ts                  # mounts App
    App.svelte               # layout: <Stage/> + <SidePanel/>
    lib/
      protocol.ts            # TS types mirroring SignalingMessage + /stream wire format
      stream.ts              # /stream socket + VideoDecoder + queue/keyframe policy
      control.ts             # /ws socket: connect, send-queue, reconnect-ready
      input.ts               # touch/pointer/wheel normalization + dispatch
      viewport.ts            # DPR + scaleFactor + /16 resize math + throttle
      stats.ts               # Svelte store: conn state, decode latency, arrival gaps, res/bitrate
    components/
      Stage.svelte           # <canvas> fill + input wiring + black bg
      SidePanel.svelte       # collapsible edge panel (tab handle)
      FullscreenButton.svelte
      StatsPanel.svelte
src/web/mod.rs               # changed: serve embedded bundle instead of one HTML string
src/web/client.html          # DELETE at the end (after parity verified)
```

## 3. Wire protocol contract — DO NOT BREAK

Mirror these exactly in `lib/protocol.ts`. Source of truth: `src/server.rs`.

- `GET /`        → HTML (will serve the new bundle's `index.html`)
- `GET /ws`      → JSON control channel (text frames). Client→server messages:
  - `{type:"ready"}` on open
  - `{type:"resize", width, height}` — **width/height must be /16-divisible**
  - `{type:"touch", eventType, touches:[{identifier,x,y,pressure}]}` — x,y ∈ [0,1]
  - `{type:"pointer", eventType, pointer:{x,y,button,pointerType,pressure}}` and
    the wheel variant `{type:"pointer", eventType:"wheel", x,y,deltaX,deltaY}`
  - `{type:"request_keyframe"}`
  - `{type:"latency", decoding_ms, total_ms, ...}`
- `GET /stream`  → binary, one WebSocket message per H.264 frame:
  - byte 0: frame_type (`0`=delta, `1`=key)
  - bytes 1–4: frame_id (u32 BE) — currently unused client-side
  - bytes 5..: Annex-B H.264

Decoder config to carry over verbatim:
`{ codec: 'avc1.42E01F', optimizeForLatency: true }` (baseline 3.1, Annex-B,
SPS/PPS inline on keyframes — no `description`).

## 4. Phase 1 — Scaffold + build/embed wiring ✅ done

- [x] `npm create vite@latest web -- --template svelte-ts`; trim to essentials.
- [x] Pin deps; keep `dependencies` empty/near-empty (Svelte/Vite are devDeps).
      No UI kit, no icon lib (inline SVG), no state lib (Svelte stores only).
- [x] `vite.config.ts`: set `build.outDir = "dist"`, **single-file-ish output**
      (let Vite hash assets; we'll embed the whole `dist/`). Add a dev `server.proxy`
      so `/ws` and `/stream` (with `ws: true`) and `/` proxy to the Rust server
      (e.g. `http://127.0.0.1:8080`) — enables `vite dev` with a live backend.
      (`/` itself is **not** proxied — that would hand Vite's own dev `index.html`
      off to the backend and break `vite dev`/HMR; only the two socket paths are.)
- [x] `.gitignore`: add `web/node_modules/` and `web/dist/`.
- [x] **Embed strategy** in Rust: add `rust-embed` dependency, embed `web/dist`.
      Add a `build.rs` that runs the Vite build (`npm ci && npm run build` in `web/`)
      so `cargo build` produces a self-contained binary. Guard it: if the `web/dist`
      already exists and sources are unchanged, skip; if `npm`/`node` is missing,
      fail with a clear message (or fall back to a committed prebuilt `dist/` —
      decide and document). Re-run on changes to `web/src/**`.
- [x] Acceptance: `cargo build` produces a binary that serves the new app at `/`;
      `cd web && npm run dev` serves the app against a running Rust backend.

  **Known regression accepted by user:** `/` now serves the (still placeholder)
  Svelte app instead of the old `client.html`, so
  `tests/integration_test.rs::test_compositor_pipeline` (Puppeteer waits for a
  `<canvas>`) fails until Phase 3/4 wire up `Stage.svelte`.

## 5. Phase 2 — Stream/decode core (`lib/stream.ts`) ✅ done

Port the *behavior* of `client.html` (it is well-tuned) into a typed module.
Preserve every comment-documented policy:

- [x] `MAX_DECODE_QUEUE = 2`; if `decoder.decodeQueueSize > MAX` and the incoming
      frame is a delta → drop it, set `keyframeSeen=false`, and `requestKeyframe()`.
- [x] If backlogged and a **key** frame arrives → `decoder.reset()` + reconfigure,
      then decode it (flush stale queue).
- [x] Drop deltas until first keyframe fed (`keyframeSeen` gate).
- [x] `requestKeyframe()` dedupe via `keyframeRequestPending`, cleared on next key.
- [x] `EncodedVideoChunk.timestamp = round(performance.now()*1000)` (µs, monotonic;
      doubles as decode-latency stamp).
- [x] On decoded frame: size canvas buffer to `frame.displayWidth/Height` **once per
      resolution change** (`canvasSized` flag, reset by viewport module on resize),
      `ctx.drawImage(frame,0,0)`, record `performance.now() - frame.timestamp/1000`,
      `frame.close()`.
- [x] Arrival-gap + queue + frame-byte diagnostics → push into `stats.ts` store
      (instead of `console.log`); keep the 5s aggregation for the latency report.
- [x] Keep `binaryType='arraybuffer'`. Clean up decoder on teardown.

  Added `lib/protocol.ts` (wire-format types/mirror of `SignalingMessage`) and
  `lib/stats.ts` (the Svelte store) as supporting pieces, both ahead of their
  nominal phases since `stream.ts` needs them to type-check. `VideoStream`
  takes an injected `sendControl` callback rather than owning `/ws` itself,
  since `lib/control.ts` doesn't exist until Phase 5 — Phase 5 just needs to
  pass its send function in.

  **Not yet wired into the app** (no `Stage.svelte`/canvas exists yet — that's
  Phase 3/4), so this is verified by `npm run check` (clean) and code review
  only, not a live browser run. Bundle size is unchanged after adding these
  files, confirming they're currently dead code pending that wiring.

## 6. Phase 3 — Viewport / scaling / resize (`lib/viewport.ts`) — THE bug fix ✅ module done, ⏳ acceptance pending Phase 4

This is the heart of the rewrite. Root cause of the old bug: resize used CSS
px (no DPR), the canvas's CSS size ended up equal to its buffer size and got
flex-centered, creating edge dead zones and a blurry sub-DPR image.

New model — **1:1 device pixel, top-left aligned, scaleFactor-aware**:

- [x] Read `dpr = window.devicePixelRatio || 1`. Define `scaleFactor` (default `1`;
      future `2` halves render resolution for hidpi perf). Expose as a store so a
      later button can flip it.
- [x] Use `window.visualViewport` when available (correct under mobile browser
      chrome / soft keyboard); fall back to `window.innerWidth/Height`.
- [x] Compute **render resolution** (sent to server), each dim:
      `render = floor(viewportCssPx * dpr / scaleFactor / 16) * 16`.
      Clamp to server `max_resolution`. This is the `{type:"resize",width,height}`.

      The server doesn't expose its configured `max_resolution` over the wire
      (and doesn't even enforce it server-side on resize requests today — see
      `src/main.rs`'s resize handling), and adding a new message would mean
      touching the protocol section 3 says not to change. So the clamp is a
      hardcoded constant mirroring the CLI default (3840x2160) — a
      conservative client-side sanity bound, not authoritative. Documented in
      `lib/viewport.ts`.
- [x] The decoded frame comes back at `render`. Set **canvas buffer** = frame size
      (handled in stream.ts). Set **canvas CSS size** = `render * scaleFactor / dpr`
      px → ≈ viewport minus the sub-16px flooring remainder. Result: sharp 1:1
      mapping with a thin margin on right/bottom (acceptable, documented).
- [x] Position canvas **top-left** of a black full-viewport container (margins sit
      bottom/right, deterministic — not centered, so input math is simple).

      `viewport.ts` only sets the canvas's CSS width/height; the actual
      top-left-in-a-black-container layout is static CSS owned by
      `Stage.svelte`, which doesn't exist yet (Phase 4).
- [x] **Throttle** resize: debounce ~300ms AND drop no-op requests (only send if the
      computed /16 dims actually changed). Fire on `resize`, `orientationchange`,
      `visualViewport` `resize`/`scroll`, and once on load. On send, reset
      `canvasSized` so the next frame re-measures the buffer.
- [ ] Acceptance: on a phone, taps land exactly under the finger across the **entire**
      visible canvas including all four edges; image is crisp (no blur); rotating
      the device and opening the side panel never offsets input.

      Now wired and exercised headlessly via Phase 4's verification
      (synthetic pointer/wheel events landed at the expected normalized
      coordinates, canvas rendered sharp at the negotiated resolution).
      Still left unchecked here pending **real phone/DPR>1 hardware**
      testing — synthetic/headless input doesn't exercise actual touch
      hardware, device rotation, or the side panel (not built yet). Real
      device pass is Phase 7's job.

## 7. Phase 4 — Input (`lib/input.ts`, wired in `Stage.svelte`) ✅ done

Carry over current handlers; normalize against the **live** canvas
`getBoundingClientRect()` every event (never a cached rect).

- [x] Touch: `touchstart/move/end/cancel`, `preventDefault`, `{passive:false}`.
      Normalize x,y to [0,1] vs canvas rect; **drop touches outside [0,1]** (margins).
      `touchend`/`cancel` use `changedTouches`.
- [x] Pointer (mouse/pen): ignore `pointerType==='touch'` (dedup vs touch handlers).
      `setPointerCapture` on down, release on up. `pointerdown/move/up/cancel`.
- [x] Wheel: `preventDefault`, send `{type:"pointer",eventType:"wheel",x,y,deltaX,deltaY}`.
- [x] `contextmenu` → `preventDefault` (right-click reaches remote).
- [x] `touch-action: none` on canvas; disable text selection / callouts on mobile.
- [x] Input must target the canvas only — the side panel must **not** forward its
      own taps as remote input (stop propagation / panel sits above canvas).

  Added `components/Stage.svelte`, the first component that actually wires
  everything together: instantiates `ControlChannel` (see below),
  `VideoStream`, `Viewport`, and `attachInput`, all sharing one
  `sendControl` closure. `App.svelte` now renders `<Stage/>` instead of the
  placeholder, so the new client is live end-to-end again.

  Also added `lib/control.ts` (`ControlChannel`: `/ws` connect, send-queue
  buffering until OPEN, `{type:"ready"}` on open), pulled forward from its
  nominal Phase 5 slot because Stage.svelte cannot assemble a working
  `sendControl` without it. Phase 5's remaining scope (pushing connection
  state into `stats.ts`, auto-reconnect, the side panel UI itself) is still
  open.

  Renamed `protocol.ts`'s `TouchEvent`/`PointerEvent` message types to
  `TouchMessage`/`PointerMessage` — they were shadowing the DOM's own
  ambient `TouchEvent`/`PointerEvent` types, which `input.ts` needs.

  Verified live in a browser via a background agent driving headless
  Chromium against the real server + a Wayland test client: canvas
  decodes and renders real (non-black) content at the negotiated
  resolution; synthetic pointerdown/move/up and wheel events produced the
  exact `/ws` JSON shapes the protocol expects and were confirmed
  server-side (`Received resize request...`, ready handshake); contextmenu
  was suppressed (`defaultPrevented === true`, no native menu); wheel
  `preventDefault` confirmed (`window.scrollY` stayed `0`); zero page
  errors (one pre-existing, unrelated `/favicon.ico` 404 console error).
  `tests/integration_test.rs::test_compositor_pipeline` (the Puppeteer
  screenshot test that needed a live canvas) now passes again — confirmed
  via `cargo test -- --test-threads=1` (full suite green). Note: that test
  and `test_compositor_startup` both hardcode the Wayland display name
  `wayland-test-0`, so they collide if run in parallel — run integration
  tests with `--test-threads=1`, or accept that default `cargo test`
  parallelism can spuriously fail them (pre-existing, unrelated to this
  phase).

## 8. Phase 5 — Control channel, side panel, fullscreen, stats ✅ done

- [x] `lib/control.ts`: `/ws` connect, send-queue buffering until OPEN (carry over),
      `{type:"ready"}` on open, push connection state into `stats.ts`. Keep
      teardown on `beforeunload`. (Auto-reconnect is **future** — leave a clean seam.)
- [x] `SidePanel.svelte`: collapsed by default, edge tab handle (noVNC style), slides
      in as an overlay on mobile (doesn't reflow/resize the canvas → no input shift),
      closes on outside tap / Esc. Accessible (button roles, focus, aria-expanded).
- [x] `FullscreenButton.svelte`: Fullscreen API on the app container; reflect state.
      **Document the iOS caveat**: iPhone Safari has no element Fullscreen API — the
      button is best-effort there; mitigate via a web app manifest
      (`display: standalone`) for add-to-home-screen. iPad/Android/desktop work.
- [x] `StatsPanel.svelte`: render from the `stats.ts` store — connection state,
      decode latency (ms), current render resolution, arrival-gap p95/max + burst
      count, max decode queue. Cheap, updates on the existing 5s cadence.

## 9. Phase 6 — Rust server integration (`src/web/mod.rs`, `src/server.rs`) ✅ done

- [x] Replace the `client_html` module with embedded-asset serving (rust-embed):
      `GET /` → `index.html`; `GET /<asset>` → hashed JS/CSS with correct
      `Content-Type` and long-cache headers (assets are content-hashed).
- [x] Keep `/ws` and `/stream` handlers **unchanged**.
- [x] Update the `serve_client` route accordingly; keep the existing server test
      (`stream_endpoint_delivers_frames_in_wire_format`) green.
- [x] Confirm `Cargo.toml` adds only `rust-embed` (+ a mime helper if needed).

  This had already landed alongside earlier phases (`src/web/mod.rs` uses
  `rust-embed` with `serve_index`/`serve_asset`; `src/server.rs`'s router is
  `/` → `serve_index`, `/ws` → `handle_websocket`, `/stream` →
  `handle_video_stream`, `.fallback(serve_asset)`; `client.html` is already
  gone; `Cargo.toml`'s only addition is `rust-embed = "8.11.0"` with the
  `mime-guess` feature). Verified clean in this pass: `cd web && npm run
  build` (46.38 kB JS / 1.50 kB CSS gzip'd), `cargo build`, and
  `cargo test -- --test-threads=1` — full suite green including
  `server::tests::stream_endpoint_delivers_frames_in_wire_format` and the
  two Puppeteer integration tests.

## 10. Phase 7 — Cleanup, docs, verification

- [x] Delete `src/web/client.html` only after the new app reaches feature parity.
      Already gone — `src/web/` contains only `mod.rs`.
- [x] Update `README.md` dev instructions (Vite dev + proxy; `cargo build` embeds).
      Updated the Architecture table's "Web client" row, added Node/npm to
      Build dependencies, and added a "Web client dev loop" section
      documenting `cargo run` + `cd web && npm run dev` side by side.
- [x] `npm run build` is clean; bundle size noted in PR (target: tiny, no framework bloat).
      `dist/index.html` 0.42 kB, `assets/index-*.css` 1.50 kB (gzip 0.67 kB),
      `assets/index-*.js` 46.38 kB (gzip 17.09 kB). `npm run check`: 0 errors,
      0 warnings across 142 files. `cargo test -- --test-threads=1`: full
      suite green.
- [ ] Manual verification with the `verify`/`run` skills on a phone or DPR>1 emulation:
      edges, sharpness, rotation, panel open/close, fullscreen, stats populate,
      latency reports still reach the adaptive-bitrate controller.
      Still open — needs real phone/DPR>1 hardware, not done in this pass.

## 11. Acceptance checklist (mobile-first)

- [ ] Taps are pixel-accurate across the whole canvas, including all edges.
- [ ] Image is sharp at 1:1 device pixels (no blur).
- [ ] Rotating the device re-fits within ~1 frame after the throttled resize; input
      stays aligned throughout.
- [ ] Opening the side panel does not shift or break input.
- [ ] Fullscreen works on Android/desktop; documented best-effort on iPhone.
- [ ] Stats panel shows live connection state + decode latency + resolution.
- [ ] Resize requests are throttled and de-duplicated (verify in server logs).
- [ ] Decoder backlog/keyframe-resync behavior matches the old client (no runaway latency).
- [ ] `cargo build` yields a single self-contained binary serving the new UI.

## 12. Explicitly deferred (design seams only, do not build now)

- 2× client-side `scaleFactor` button (the math already supports it).
- Manual reconnect button + auto-reconnect with backoff.
- Force-keyframe button (protocol already supports `request_keyframe`).
- On-screen keyboard / key forwarding (**needs a new server input path** — no
  key event type exists in the protocol yet).
- WebGL/WebGPU renderer instead of 2D `drawImage` (only if profiling shows the
  2D path is a bottleneck).
