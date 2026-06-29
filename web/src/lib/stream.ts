// H.264 VideoDecoder + canvas blit. The transport (the `/client` unified
// WebSocket) lives in lib/client.ts; this module consumes `VideoFramePayload`
// objects that client.ts hands it, decodes them via WebCodecs, and paints
// them onto the supplied canvas. Keeping the decoder logic here, separate
// from the socket owner, mirrors how the Rust server splits the encoder
// thread off from the broadcast stream.
import { createVideoRenderer, type RenderCanvas, type RendererBackend, type VideoRenderer } from './glRenderer';
import {
  DECODER_CONFIG,
  type ClientMessage,
  type VideoFramePayload,
} from './protocol';

// Diagnostics sinks, injected rather than imported from ./stats directly so
// the same VideoStream can run on the main thread (sinks = the stats.ts store
// updaters) or inside the Stage B worker (sinks = postMessage to the main
// thread, which then updates the store). Keeping ./stats out of this module
// also keeps svelte/store out of the worker bundle.
export interface ArrivalStats {
  avgMs: number;
  p95Ms: number;
  maxMs: number;
  burstCount: number;
  maxQueue: number;
  maxFrameBytes: number;
}
export interface DecodeStats {
  decodeAvgMs: number;
  blitAvgMs: number;
  blitP95Ms: number;
}
export interface StreamReporters {
  setResolution(width: number, height: number): void;
  reportArrivalStats(stats: ArrivalStats): void;
  reportDecodeStats(stats: DecodeStats): void;
  reportEndToEndLatency(ms: number): void;
}

// VideoDecoder's internal decode queue has no cap of its own: if frames
// arrive faster than they can be decoded (bursty/slow remote network,
// decode contention), the backlog grows without bound and every queued
// frame's reported latency climbs forever. Mirror the server's "skip to
// newest, resync on next keyframe" policy (src/server.rs) here too.
const MAX_DECODE_QUEUE = 2;

const DIAGNOSTICS_INTERVAL_MS = 5000;

// How often to probe round-trip latency (network + whole server pipeline).
// Faster than DIAGNOSTICS_INTERVAL_MS so a few samples land in every
// reporting window.
const PING_INTERVAL_MS = 1000;

// Resizing the canvas *bitmap* (not its CSS size) on every frame whose
// dimensions actually changed -- rather than once, gated by an external
// "did we just request a resize" signal -- is what makes this self-healing:
// the very first decoded frame can land at the server's old/default
// resolution before a just-sent resize takes effect. A one-shot flag would
// latch onto that stale size forever (stretching every later,
// correctly-sized frame); this instead just keeps comparing against the
// frame actually in hand.
export function ensureCanvasSize(
  canvas: RenderCanvas,
  width: number,
  height: number,
): boolean {
  if (canvas.width === width && canvas.height === height) {
    return false;
  }
  canvas.width = width;
  canvas.height = height;
  return true;
}

export interface VideoStreamOptions {
  canvas: RenderCanvas;
  /// Used to send `request_keyframe` and periodic `latency` reports. The
  /// transport is owned by lib/client.ts; this is just the typed send
  /// callback so we don't reach back into ClientChannel from here.
  sendControl: (msg: ClientMessage) => void;
  /// Where diagnostics go (resolution, arrival/decode/end-to-end stats). See
  /// `StreamReporters` -- main-thread wiring passes the ./stats updaters; the
  /// worker passes postMessage shims.
  reporters: StreamReporters;
}

export class VideoStream {
  private readonly canvas: RenderCanvas;
  private readonly renderer: VideoRenderer;
  private readonly sendControl: (msg: ClientMessage) => void;
  private readonly reporters: StreamReporters;

  private decoder: VideoDecoder | null = null;
  // Updated via `setCodec` when the server reports a new H.264 level (e.g.
  // after a resolution change); DECODER_CONFIG is just the startup default.
  private codecConfig: VideoDecoderConfig = DECODER_CONFIG;

  // Drop deltas until the first keyframe is fed: a delta decoded without a
  // preceding keyframe has no reference picture to diff against.
  private keyframeSeen = false;
  // Dedupes resync requests; cleared once a keyframe arrives.
  private keyframeRequestPending = false;

  private lastArrivalTime: number | null = null;
  private arrivalGapSamples: number[] = [];
  private maxQueueSeenInWindow = 0;
  private maxFrameBytesInWindow = 0;
  private decodeLatencySamples: number[] = [];
  private blitLatencySamples: number[] = [];
  private diagnosticsTimer: ReturnType<typeof setInterval> | null = null;

  // Round-trip latency samples, one per echoed ping (see `sendPing` and the
  // `pingEchoClientTs` handling below). Each echo carries back exactly the
  // `performance.now()` value this client sent it with, so `rtt = now -
  // echo` needs no clock sync between client and server.
  private rttSamples: number[] = [];
  // Sticky across reporting windows so a quiet window (no ping happened to
  // land) still shows the last real measurement instead of dropping to 0.
  private lastRttMs = 0;
  // Sticky for the same reason as lastRttMs -- reported to the server
  // alongside decode latency so it can detect network-level bufferbloat
  // (bursty arrival with a shallow decode queue) that the decode-queue
  // depth check above can't see on its own. Burst count, not arrival-gap
  // p95: on an idle screen the server only sends a frame every
  // keyframe_interval ticks (no damage to capture), so a long gap there
  // is expected silence, not a stall -- p95 would false-positive on every
  // idle period. A burst (several frames landing within ~3ms of each
  // other) can only happen if frames actually piled up somewhere and got
  // released together, which idle periods can't produce since there's
  // nothing queued to release.
  private lastBurstCount = 0;
  private pingTimer: ReturnType<typeof setInterval> | null = null;

  constructor(opts: VideoStreamOptions) {
    this.canvas = opts.canvas;
    this.renderer = createVideoRenderer(this.canvas);
    this.sendControl = opts.sendControl;
    this.reporters = opts.reporters;
  }

  /// The live render backend ('webgl' / 'webgl2' / '2d'). Surfaced so the
  /// pipeline coordinator can report it to the stats panel -- on the worker
  /// path this is read inside the worker and posted across.
  get rendererBackend(): RendererBackend {
    return this.renderer.backend;
  }

  /// Called by lib/client.ts when the socket opens. Idempotent: a fresh
  /// decoder is created so any leftover state from a dropped connection
  /// (queued chunks in flight, half-configured decoder) is discarded.
  start(): void {
    this.setupDecoder();
    this.diagnosticsTimer = setInterval(() => this.flushDiagnostics(), DIAGNOSTICS_INTERVAL_MS);
    this.pingTimer = setInterval(() => this.sendPing(), PING_INTERVAL_MS);
    // A fresh session starts a brand-new frame sequence server-side, so
    // drop everything tracked about the old one: a stale `keyframeSeen`/
    // pending request would misroute frames from the new sequence, and the
    // gap left by the outage itself isn't congestion (would otherwise read
    // as exactly that on the first frame back).
    this.keyframeSeen = false;
    this.keyframeRequestPending = false;
    this.lastArrivalTime = null;
  }

  close(): void {
    if (this.diagnosticsTimer !== null) {
      clearInterval(this.diagnosticsTimer);
      this.diagnosticsTimer = null;
    }
    if (this.pingTimer !== null) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
    if (this.decoder && this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    this.decoder = null;
  }

  private setupDecoder(): void {
    this.decoder = new VideoDecoder({
      output: (frame) => this.handleFrame(frame),
      error: (e) => {
        console.error('Decoder error:', e);
        this.recoverDecoder();
      },
    });
    this.decoder.configure(this.codecConfig);
  }

  /// Per the WebCodecs spec, a decoder that reports an error transitions to
  /// 'closed' permanently -- reset()/configure() on it would throw. The only
  /// way back is a brand-new VideoDecoder instance, then resync from the
  /// next keyframe since whatever reference state the old one had is gone.
  private recoverDecoder(): void {
    this.setupDecoder();
    this.keyframeSeen = false;
    this.requestKeyframe();
  }

  /// Called when the server reports a new WebCodecs codec string (see
  /// ServerMessage), e.g. because a resolution change picked a different
  /// H.264 level. Unlike `recoverDecoder`, the decoder here is still healthy,
  /// so reset()+configure() (the same pattern already used below for
  /// backlog flushes) is enough -- no need to replace the instance.
  setCodec(codec: string): void {
    if (this.codecConfig.codec === codec) return;
    this.codecConfig = { ...this.codecConfig, codec };
    if (this.decoder && this.decoder.state !== 'closed') {
      this.decoder.reset();
      this.decoder.configure(this.codecConfig);
    }
    // The server emits a fresh IDR with the new SPS right after switching
    // levels, but the codec update and the matching frame are independent
    // messages on the same socket -- request one explicitly rather than
    // racing them.
    this.keyframeSeen = false;
    this.requestKeyframe();
  }

  private handleFrame(frame: VideoFrame): void {
    if (ensureCanvasSize(this.canvas, frame.displayWidth, frame.displayHeight)) {
      this.reporters.setResolution(frame.displayWidth, frame.displayHeight);
    }
    // Stamp before the blit so decode latency excludes it. `timestamp` is the
    // performance.now()*1000 set on the chunk in `handleVideoFrame`.
    const decodeDoneMs = performance.now();
    this.decodeLatencySamples.push(decodeDoneMs - frame.timestamp / 1000);
    // A VideoFrame must be closed exactly once; an un-closed frame holds a slot
    // in the decoder's output pool and will stall decoding once the pool is
    // exhausted. So close it even if the blit throws (e.g. a transient WebGL
    // error) -- losing one frame to a failed draw is fine, leaking it is not.
    try {
      this.renderer.draw(frame);
      // With the WebGL backend this is just the texture upload + draw-call
      // submission (the GPU work is async), so it should stay near zero. A
      // large value means the 2D fallback is active and its VideoFrame→canvas
      // readback is the bottleneck, not the decoder.
      this.blitLatencySamples.push(performance.now() - decodeDoneMs);
    } finally {
      frame.close();
    }
  }

  private sendPing(): void {
    this.sendControl({ type: 'ping', client_ts: performance.now() });
  }

  private requestKeyframe(): void {
    if (this.keyframeRequestPending) return;
    this.keyframeRequestPending = true;
    this.sendControl({ type: 'request_keyframe' });
  }

  /// Called by lib/client.ts for every MSG_VIDEO_FRAME it dispatches.
  /// Stateless w.r.t. the socket: the same payload sequence arriving over a
  /// freshly reconnected socket should produce the same decoded output as
  /// the old one.
  handleVideoFrame(payload: VideoFramePayload): void {
    const decoder = this.decoder;
    if (!decoder) return;

    const arrivalNow = performance.now();
    if (this.lastArrivalTime !== null) {
      this.arrivalGapSamples.push(arrivalNow - this.lastArrivalTime);
    }
    this.lastArrivalTime = arrivalNow;
    // Recorded regardless of what happens to this frame below (dropped for
    // backlog, gated pending a keyframe, etc.) -- it's measuring this
    // frame's transit time, not whether we end up decoding it.
    if (payload.pingEchoClientTs !== null) {
      this.rttSamples.push(arrivalNow - payload.pingEchoClientTs);
    }
    this.maxQueueSeenInWindow = Math.max(this.maxQueueSeenInWindow, decoder.decodeQueueSize);
    this.maxFrameBytesInWindow = Math.max(
      this.maxFrameBytesInWindow,
      20 + payload.data.byteLength,
    );

    if (decoder.decodeQueueSize > MAX_DECODE_QUEUE) {
      if (!payload.isKeyframe) {
        // Already backlogged -- drop this delta rather than add to the
        // queue, and ask for a fresh keyframe to resync instead of waiting
        // out the rest of the GOP.
        this.keyframeSeen = false;
        this.requestKeyframe();
        return;
      }
      // A keyframe arrived while backlogged: flush the decoder so it
      // catches up immediately instead of working through stale frames
      // already queued.
      decoder.reset();
      decoder.configure(this.codecConfig);
    }

    if (!payload.isKeyframe && !this.keyframeSeen) {
      return;
    }
    if (payload.isKeyframe) {
      this.keyframeSeen = true;
      this.keyframeRequestPending = false;
    }

    const chunk = new EncodedVideoChunk({
      type: payload.isKeyframe ? 'key' : 'delta',
      // No presentation clock to sync to here; arrival time just needs to
      // be monotonic microseconds, and doubles as a decode-latency stamp.
      timestamp: Math.round(performance.now() * 1000),
      data: payload.data,
    });

    try {
      decoder.decode(chunk);
    } catch (e) {
      console.error('Decode error:', e);
      this.recoverDecoder();
    }
  }

  private flushDiagnostics(): void {
    if (this.arrivalGapSamples.length > 0) {
      const sorted = [...this.arrivalGapSamples].sort((a, b) => a - b);
      const n = sorted.length;
      const avgMs = this.arrivalGapSamples.reduce((a, b) => a + b, 0) / n;
      const maxMs = sorted[n - 1];
      const p95Ms = sorted[Math.floor(n * 0.95)];
      // A "burst" is several messages arriving within a couple ms of each
      // other -- much tighter than one frame interval, so it means a batch
      // was released all at once rather than delivered at a steady cadence.
      const burstCount = this.arrivalGapSamples.filter((g) => g < 3).length;
      this.lastBurstCount = burstCount;

      this.reporters.reportArrivalStats({
        avgMs,
        p95Ms,
        maxMs,
        burstCount,
        maxQueue: this.maxQueueSeenInWindow,
        maxFrameBytes: this.maxFrameBytesInWindow,
      });

      this.arrivalGapSamples = [];
      this.maxQueueSeenInWindow = 0;
      this.maxFrameBytesInWindow = 0;
    }

    if (this.rttSamples.length > 0) {
      this.lastRttMs = this.rttSamples.reduce((a, b) => a + b, 0) / this.rttSamples.length;
      this.rttSamples = [];
    }

    let blitAvgMs = 0;
    let blitP95Ms = 0;
    if (this.blitLatencySamples.length > 0) {
      const sorted = [...this.blitLatencySamples].sort((a, b) => a - b);
      const n = sorted.length;
      blitAvgMs = this.blitLatencySamples.reduce((a, b) => a + b, 0) / n;
      blitP95Ms = sorted[Math.floor(n * 0.95)];
      this.blitLatencySamples = [];
    }

    if (this.decodeLatencySamples.length === 0) return;
    const decodeN = this.decodeLatencySamples.length;
    const decodeAvgMs =
      this.decodeLatencySamples.reduce((a, b) => a + b, 0) / decodeN;
    this.decodeLatencySamples = [];

    this.reporters.reportDecodeStats({ decodeAvgMs, blitAvgMs, blitP95Ms });

    // Glass-to-glass: ping round-trip (network + capture→encode→send on the
    // server, all folded in -- see the wire format doc in protocol.ts) plus
    // this client's own decode time.
    const totalMs = this.lastRttMs + decodeAvgMs;
    this.reporters.reportEndToEndLatency(totalMs);
    this.sendControl({
      type: 'latency',
      network_ms: this.lastRttMs,
      decoding_ms: decodeAvgMs,
      total_ms: totalMs,
      burst_count: this.lastBurstCount,
      blit_ms: blitAvgMs > 0 ? blitAvgMs : undefined,
    });
  }
}