// Svelte store for connection/decode diagnostics, fed by stream.ts and
// control.ts, and rendered by StatsPanel.svelte.
import { writable } from 'svelte/store';

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed' | 'error';

export interface CursorDebug {
  kind: string;         // last received cursor kind
  count: number;        // total cursor messages received
  overlayDisplay: string; // current cursorOverlay.style.display
  overlayTransform: string; // current cursorOverlay.style.transform
  imgW: number; imgH: number; // overlay dimensions
}

export interface StreamStats {
  connectionState: ConnectionState;
  cursorDebug: CursorDebug | null;
  /// Glass-to-glass: ping round-trip (network + whole server pipeline,
  /// measured via the embedded-timestamp ping/echo in stream.ts) plus this
  /// client's own decode time. See `VideoStream.flushDiagnostics`.
  endToEndLatencyMs: number;
  resolution: { width: number; height: number } | null;
  arrivalGapAvgMs: number;
  arrivalGapP95Ms: number;
  arrivalGapMaxMs: number;
  burstCount: number;
  maxDecodeQueue: number;
  maxFrameBytes: number;
  /// Current encoder target bitrate reported by the server over `/ws`. 0
  /// means "not yet known" or "not applicable" (constant-quality/CRF mode).
  bitrateBps: number;
  /// Average time from feeding a chunk to `decoder.decode()` until its frame
  /// surfaces in the `output` callback (decoder work only, blit excluded).
  decodeAvgMs: number;
  /// Average wall-clock cost of `ctx.drawImage(VideoFrame)` alone.
  blitAvgMs: number;
  /// 95th-percentile blit cost; catches occasional expensive blits a mean
  /// would smooth away (e.g. the first frame after a tab-refocus).
  blitP95Ms: number;
}

const initialStats: StreamStats = {
  connectionState: 'connecting',
  cursorDebug: null,
  endToEndLatencyMs: 0,
  resolution: null,
  arrivalGapAvgMs: 0,
  arrivalGapP95Ms: 0,
  arrivalGapMaxMs: 0,
  burstCount: 0,
  maxDecodeQueue: 0,
  maxFrameBytes: 0,
  bitrateBps: 0,
  decodeAvgMs: 0,
  blitAvgMs: 0,
  blitP95Ms: 0,
};

export const streamStats = writable<StreamStats>(initialStats);

export function setConnectionState(state: ConnectionState): void {
  streamStats.update((s) => ({ ...s, connectionState: state }));
}

export function setResolution(width: number, height: number): void {
  streamStats.update((s) => ({ ...s, resolution: { width, height } }));
}

export function reportEndToEndLatency(ms: number): void {
  streamStats.update((s) => ({ ...s, endToEndLatencyMs: ms }));
}

export function setCursorDebug(d: CursorDebug): void {
  streamStats.update((s) => ({ ...s, cursorDebug: d }));
}

export function setBitrate(bps: number): void {
  streamStats.update((s) => ({ ...s, bitrateBps: bps }));
}

export function reportDecodeStats(stats: {
  decodeAvgMs: number;
  blitAvgMs: number;
  blitP95Ms: number;
}): void {
  streamStats.update((s) => ({
    ...s,
    decodeAvgMs: stats.decodeAvgMs,
    blitAvgMs: stats.blitAvgMs,
    blitP95Ms: stats.blitP95Ms,
  }));
}

export function reportArrivalStats(stats: {
  avgMs: number;
  p95Ms: number;
  maxMs: number;
  burstCount: number;
  maxQueue: number;
  maxFrameBytes: number;
}): void {
  streamStats.update((s) => ({
    ...s,
    arrivalGapAvgMs: stats.avgMs,
    arrivalGapP95Ms: stats.p95Ms,
    arrivalGapMaxMs: stats.maxMs,
    burstCount: stats.burstCount,
    maxDecodeQueue: stats.maxQueue,
    maxFrameBytes: stats.maxFrameBytes,
  }));
}
