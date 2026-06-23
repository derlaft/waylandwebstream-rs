// Connects to `/stream`, decodes H.264 via WebCodecs, and paints onto a
// canvas. Ported behavior-for-behavior from the old src/web/client.html
// (see git history) into a typed module; see comments below for the policy
// each piece of logic encodes.
import {
  DECODER_CONFIG,
  STREAM_FRAME_HEADER_BYTES,
  parseStreamFrameHeader,
  type ClientMessage,
} from './protocol';
import { reportArrivalStats, reportEndToEndLatency, setResolution } from './stats';

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
// the /stream and /ws sockets connect independently, so the very first
// decoded frame can land at the server's old/default resolution before a
// just-sent resize takes effect. A one-shot flag would latch onto that
// stale size forever (stretching every later, correctly-sized frame); this
// instead just keeps comparing against the frame actually in hand.
export function ensureCanvasSize(
  canvas: HTMLCanvasElement,
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
  canvas: HTMLCanvasElement;
  /// Used to send `request_keyframe` and periodic `latency` reports over
  /// the `/ws` control channel. Decoupled from a concrete socket here
  /// because lib/control.ts (the /ws owner) lands in a later phase.
  sendControl: (msg: ClientMessage) => void;
}

export class VideoStream {
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  private readonly sendControl: (msg: ClientMessage) => void;

  private ws: WebSocket | null = null;
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
  // `keyframe_interval` ticks (no damage to capture), so a long gap there
  // is expected silence, not a stall -- p95 would false-positive on every
  // idle period. A burst (several frames landing within ~3ms of each
  // other) can only happen if frames actually piled up somewhere and got
  // released together, which idle periods can't produce since there's
  // nothing queued to release.
  private lastBurstCount = 0;
  private pingTimer: ReturnType<typeof setInterval> | null = null;

  constructor(opts: VideoStreamOptions) {
    this.canvas = opts.canvas;
    const ctx = this.canvas.getContext('2d');
    if (!ctx) {
      throw new Error('2D canvas context unavailable');
    }
    this.ctx = ctx;
    this.sendControl = opts.sendControl;
  }

  connect(): void {
    this.setupDecoder();
    this.connectSocket();
    this.diagnosticsTimer = setInterval(() => this.flushDiagnostics(), DIAGNOSTICS_INTERVAL_MS);
    this.pingTimer = setInterval(() => this.sendPing(), PING_INTERVAL_MS);
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
    this.ws?.close();
    this.ws = null;
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
    // levels, but `/ws` (this message) and `/stream` (the frame) are
    // independent sockets -- request one explicitly rather than racing it.
    this.keyframeSeen = false;
    this.requestKeyframe();
  }

  private handleFrame(frame: VideoFrame): void {
    if (ensureCanvasSize(this.canvas, frame.displayWidth, frame.displayHeight)) {
      setResolution(frame.displayWidth, frame.displayHeight);
    }
    this.ctx.drawImage(frame, 0, 0);
    this.decodeLatencySamples.push(performance.now() - frame.timestamp / 1000);
    frame.close();
  }

  private sendPing(): void {
    this.sendControl({ type: 'ping', client_ts: performance.now() });
  }

  private requestKeyframe(): void {
    if (this.keyframeRequestPending) return;
    this.keyframeRequestPending = true;
    this.sendControl({ type: 'request_keyframe' });
  }

  private connectSocket(): void {
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/stream`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    ws.onmessage = (event) => this.onStreamMessage(event.data as ArrayBuffer);
    ws.onerror = (e) => console.error('Video stream error:', e);
    this.ws = ws;
  }

  private onStreamMessage(buf: ArrayBuffer): void {
    const decoder = this.decoder;
    if (!decoder) return;

    const { isKeyframe, pingEchoClientTs } = parseStreamFrameHeader(buf);

    const arrivalNow = performance.now();
    if (this.lastArrivalTime !== null) {
      this.arrivalGapSamples.push(arrivalNow - this.lastArrivalTime);
    }
    this.lastArrivalTime = arrivalNow;
    // Recorded regardless of what happens to this frame below (dropped for
    // backlog, gated pending a keyframe, etc.) -- it's measuring this
    // frame's transit time, not whether we end up decoding it.
    if (pingEchoClientTs !== null) {
      this.rttSamples.push(arrivalNow - pingEchoClientTs);
    }
    this.maxQueueSeenInWindow = Math.max(this.maxQueueSeenInWindow, decoder.decodeQueueSize);
    this.maxFrameBytesInWindow = Math.max(this.maxFrameBytesInWindow, buf.byteLength);

    if (decoder.decodeQueueSize > MAX_DECODE_QUEUE) {
      if (!isKeyframe) {
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

    const data = new Uint8Array(buf, STREAM_FRAME_HEADER_BYTES);

    if (!isKeyframe && !this.keyframeSeen) {
      return;
    }
    if (isKeyframe) {
      this.keyframeSeen = true;
      this.keyframeRequestPending = false;
    }

    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      // No presentation clock to sync to here; arrival time just needs to
      // be monotonic microseconds, and doubles as a decode-latency stamp.
      timestamp: Math.round(performance.now() * 1000),
      data,
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

      reportArrivalStats({
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

    if (this.decodeLatencySamples.length === 0) return;
    const decodeAvgMs =
      this.decodeLatencySamples.reduce((a, b) => a + b, 0) / this.decodeLatencySamples.length;
    this.decodeLatencySamples = [];

    // Glass-to-glass: ping round-trip (network + capture→encode→send on the
    // server, all folded in -- see the wire format doc in protocol.ts) plus
    // this client's own decode time.
    const totalMs = this.lastRttMs + decodeAvgMs;
    reportEndToEndLatency(totalMs);
    this.sendControl({
      type: 'latency',
      network_ms: this.lastRttMs,
      decoding_ms: decodeAvgMs,
      total_ms: totalMs,
      burst_count: this.lastBurstCount,
    });
  }
}
