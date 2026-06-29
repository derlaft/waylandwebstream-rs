# TODO

Genuinely-remaining work, verified against the current source (not the stale
plan docs). Roughly ordered by impact. See `AGENTS.md` for architecture and the
constraints each item has to respect.

## Server robustness / hardening

- **Advertise `max_resolution` to the client (optional).** Server-side
  enforcement is done (`sanitize_resolution` clamps every resize request), but
  the server doesn't advertise its configured max over `/client`, so the client
  still uses a hardcoded 3840×2160 safety-net cap (`web/src/lib/viewport.ts`).
  Pushing the real bound on connect would let the client cap correctly when an
  admin sets `--max-resolution` below that. Marginal — the server clamps
  regardless, so this only saves a wasted round-trip.

## Multi-client

Single-controller is enforced (one client at a time via generation gating), so
input arbitration is settled by that decision. Still notional if this ever grows
past viewing-only:
- **Resize conflicts** — a second controller would change resolution for
  everyone (shared output + encoder).
- An adaptive-bitrate cut for one struggling client would lower quality for all.

These only matter if the single-controller policy is ever relaxed
(per-client virtual output, observer mode, etc.).

## Client / UI polish (design seams already in place)

- **Manual force-keyframe button** in `SidePanel.svelte` (protocol already
  supports `request_keyframe`; auto-reconnect already exists).

## Performance (optional, measure first)

- **Damage-driven partial repaint.** Frame-level damage *gating* is done, but
  `render()` still `fill(0)`s and repaints every window each frame it does run.
  With the accumulated damage rect already available, only the damaged region
  needs repainting. Optional — the encoder already sees a full frame either way.
- **A/B measurement** against Selkies: quantify the frame-pacing and
  jitter-buffer behavior on identical content (never formally measured).

## Non-goals (deliberately out of scope)

- **Authentication / authorization.** This project does **not** and will not
  implement auth. Run it behind a reverse proxy (nginx, Caddy, oauth2-proxy,
  etc.) and let that handle authentication, TLS/`wss://`, and access control.
  Bind to `127.0.0.1` via `--listen-addr` so only the local proxy can reach the
  server directly. Do not add login/token/session handling here.
- **Metrics endpoint.** Not wanted — encode latency, bitrate, drops, and active
  clients stay in the logs.
- **Containerization** (Dockerfile). Not wanted yet.

## Done (kept here briefly so they stop getting re-added as "missing")

Audio (PipeWire → Opus over `/client`); cursor rendering (shape + custom
surface forwarded to a client-side overlay); on-screen keyboard; bidirectional
text + PNG clipboard sync; single-controller enforcement + manual reconnect;
HiDPI native-resolution toggle; WebGL renderer; the 2× `scaleFactor` plumbing;
server-side `max_resolution` clamping; logging-level cleanup (quiet default
`RUST_LOG=info`, hot-path chatter demoted to `debug`).
