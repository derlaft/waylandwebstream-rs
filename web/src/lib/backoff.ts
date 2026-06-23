// Exponential backoff with full jitter (delay is drawn uniformly from
// [0, ceiling], not ceiling itself) for WebSocket reconnects. Full jitter
// rather than a fixed delay plus small jitter: it spreads retries out much
// more evenly, which matters here because both `/ws` and `/stream` go down
// together on, say, a server restart -- without it every client would
// hammer the server in lockstep on the same retry schedule.
const INITIAL_DELAY_MS = 500;
const MAX_DELAY_MS = 15_000;

export function nextBackoffDelayMs(attempt: number): number {
  const ceiling = Math.min(MAX_DELAY_MS, INITIAL_DELAY_MS * 2 ** attempt);
  return Math.random() * ceiling;
}
