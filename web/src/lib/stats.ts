// Svelte store for connection/decode diagnostics, fed by stream.ts (and
// later control.ts/viewport.ts) and rendered by StatsPanel.svelte.
import { writable } from 'svelte/store';

export interface StreamStats {
  decodeLatencyMs: number;
  resolution: { width: number; height: number } | null;
  arrivalGapAvgMs: number;
  arrivalGapP95Ms: number;
  arrivalGapMaxMs: number;
  burstCount: number;
  maxDecodeQueue: number;
  maxFrameBytes: number;
}

const initialStats: StreamStats = {
  decodeLatencyMs: 0,
  resolution: null,
  arrivalGapAvgMs: 0,
  arrivalGapP95Ms: 0,
  arrivalGapMaxMs: 0,
  burstCount: 0,
  maxDecodeQueue: 0,
  maxFrameBytes: 0,
};

export const streamStats = writable<StreamStats>(initialStats);

export function setResolution(width: number, height: number): void {
  streamStats.update((s) => ({ ...s, resolution: { width, height } }));
}

export function reportDecodeLatency(ms: number): void {
  streamStats.update((s) => ({ ...s, decodeLatencyMs: ms }));
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
