import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ControlChannel } from './control';
import { FakeWebSocket, installFakeWebSocket } from './fakeWebSocket';

describe('ControlChannel reconnect', () => {
  beforeEach(() => {
    installFakeWebSocket();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('reconnects after an unexpected close, growing the delay each time and resetting it after a successful open', () => {
    // Full jitter draws uniformly from [0, ceiling]; pinning Math.random to
    // 1 makes the delay land on the ceiling itself, so the exact growth
    // curve (and the reset) is asserted precisely below.
    vi.spyOn(Math, 'random').mockReturnValue(1);
    const setTimeoutSpy = vi.spyOn(globalThis, 'setTimeout');

    const channel = new ControlChannel();
    channel.connect();
    expect(FakeWebSocket.instances).toHaveLength(1);

    FakeWebSocket.instances[0].simulateClose();
    expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 500);
    // Backoff is pending -- no new socket until the timer fires.
    expect(FakeWebSocket.instances).toHaveLength(1);

    vi.runOnlyPendingTimers();
    expect(FakeWebSocket.instances).toHaveLength(2);

    // A second unexpected close without an intervening open should grow
    // the delay (500ms -> 1000ms).
    FakeWebSocket.instances[1].simulateClose();
    expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 1000);

    vi.runOnlyPendingTimers();
    expect(FakeWebSocket.instances).toHaveLength(3);

    // A successful open resets the attempt counter, so the next close
    // backs off from the start again (1000ms -> 500ms, not 2000ms).
    FakeWebSocket.instances[2].simulateOpen();
    FakeWebSocket.instances[2].simulateClose();
    expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 500);
  });

  it('does not reconnect after an intentional close()', () => {
    const channel = new ControlChannel();
    channel.connect();
    channel.close();

    vi.runAllTimers();
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it('flushes queued sends once a reconnect succeeds', () => {
    const channel = new ControlChannel();
    channel.connect();
    FakeWebSocket.instances[0].simulateOpen();

    FakeWebSocket.instances[0].simulateClose();
    channel.send({ type: 'request_keyframe' });
    vi.runOnlyPendingTimers();

    const reconnected = FakeWebSocket.instances[1];
    expect(reconnected.sent).toHaveLength(0);
    reconnected.simulateOpen();
    // `ready` (sent on every open) plus the queued message from while
    // disconnected.
    expect(reconnected.sent.map((s) => JSON.parse(s).type)).toEqual(['ready', 'request_keyframe']);
  });
});
