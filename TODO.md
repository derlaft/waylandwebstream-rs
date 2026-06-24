# TODO

Genuinely-remaining work, verified against the current source (not the stale
plan docs). Roughly ordered by impact. See `AGENTS.md` for architecture and the
constraints each item has to respect.

## Features not yet implemented

- **Audio.** No audio path exists. Plan was PipeWire/PulseAudio monitor capture
  → Opus → a second stream. Entirely greenfield.
- **Cursor rendering.** The compositor composites no cursor into the
  framebuffer (`cursor_image` in `compositor/state.rs` is a no-op), so the
  remote viewer never sees a pointer. Either composite the cursor image into
  `render()` or send cursor position/shape over `/ws` for a client-side overlay.
- **On-screen (virtual) keyboard for touch devices.** Physical-keyboard
  forwarding is fully wired (browser `KeyboardEvent.code` → evdev → seat), but
  there's no on-screen keyboard for phones/tablets with no hardware keyboard.
- **Hardware encoding (VAAPI/NVENC).** Software x264 only. Would need encoder
  selection/fallback in `src/encoder/mod.rs`.

## Multi-client (currently single-controller in practice)

One encoder broadcasts to all `/stream` clients, but:
- **No input arbitration** — every connected client injects into the same seat
  with no controller/observer distinction.
- **Resize conflicts unhandled** — the last client to send `resize` wins and
  changes the resolution for everyone (shared output + encoder).
- An adaptive-bitrate cut for one struggling client lowers quality for all.

Decide a policy (single-master / largest-wins / per-client virtual output)
before this is more than a viewing-only multi-client.

## Server robustness / hardening

- **`max_resolution` is not enforced server-side.** `--max-resolution` is
  parsed but the resize handler in `main.rs` doesn't clamp to it; the client
  clamps to a hardcoded 3840×2160 instead. Enforce on the server and consider
  advertising the real bound over `/ws` so the client stops guessing.
- **No resize-spam guard.** Nothing server-side bounds resize-request rate (the
  client debounces, but a hostile client could spam encoder reinits). A simple
  rate limit is cheap insurance. (Note: this is DoS hardening, *not* auth — see
  "Non-goals" below.)
- **No metrics endpoint** (encode latency, bitrate, drops, active clients are
  logged but not exported).
- **Containerization** (Dockerfile with FFmpeg + Wayland libs) doesn't exist.

## Non-goals (deliberately out of scope)

- **Authentication / authorization.** This project does **not** and will not
  implement auth. Run it behind a reverse proxy (nginx, Caddy, oauth2-proxy,
  etc.) and let that handle authentication, TLS/`wss://`, and access control.
  Bind to `127.0.0.1` via `--listen-addr` so only the local proxy can reach the
  server directly. Do not add login/token/session handling here.

## Client / UI polish (design seams already in place)

- **2× `scaleFactor` button.** The `scaleFactor` store (`web/src/lib/
  viewport.ts`, default 1) and all the `/16` math already support halving
  render resolution for hidpi perf — just no UI to flip it.
- **Manual force-keyframe and reconnect buttons** in `SidePanel.svelte`
  (protocol supports `request_keyframe`; auto-reconnect already exists).
- **WebGL/WebGPU renderer** instead of 2D `drawImage` — only if profiling shows
  the 2D blit is a bottleneck.

## Performance (optional, measure first)

- **Damage-driven partial repaint.** Frame-level damage *gating* is done, but
  `render()` still `fill(0)`s and repaints every window each frame it does run.
  With the accumulated damage rect already available, only the damaged region
  needs repainting. Optional — the encoder already sees a full frame either way.
- **A/B measurement** against Selkies: quantify the frame-pacing and
  jitter-buffer behavior on identical content (never formally measured).

## Verification debt

- **Real mobile / DPR>1 hardware pass.** The UI rewrite's edge-accuracy,
  sharpness, rotation, side-panel-doesn't-shift-input, and fullscreen
  acceptance criteria were only checked headlessly/synthetically. Needs a real
  phone with `devicePixelRatio > 1`.
