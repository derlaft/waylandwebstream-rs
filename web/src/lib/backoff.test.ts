import { afterEach, describe, expect, it, vi } from 'vitest';
import { nextBackoffDelayMs } from './backoff';

describe('nextBackoffDelayMs', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('scales the jitter ceiling exponentially with attempt count', () => {
    // Full jitter draws uniformly from [0, ceiling]; pinning Math.random to
    // 1 makes the draw land on the ceiling itself, so this also pins down
    // the exact growth curve.
    vi.spyOn(Math, 'random').mockReturnValue(1);
    expect(nextBackoffDelayMs(0)).toBe(500);
    expect(nextBackoffDelayMs(1)).toBe(1000);
    expect(nextBackoffDelayMs(2)).toBe(2000);
  });

  it('caps the ceiling so backoff cannot grow unbounded', () => {
    vi.spyOn(Math, 'random').mockReturnValue(1);
    expect(nextBackoffDelayMs(10)).toBe(15_000);
  });

  it('can draw down to zero when jitter lands at the minimum', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0);
    expect(nextBackoffDelayMs(5)).toBe(0);
  });
});
