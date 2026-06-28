import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ClipboardBridge, clipboardSyncEnabled } from './clipboard';
import type { ClientMessage } from './protocol';

let writeText: ReturnType<typeof vi.fn>;
let readText: ReturnType<typeof vi.fn>;
let bridge: ClipboardBridge | null = null;

function setup(): { sent: ClientMessage[]; bridge: ClipboardBridge } {
  const sent: ClientMessage[] = [];
  bridge = new ClipboardBridge((m) => sent.push(m));
  return { sent, bridge };
}

beforeEach(() => {
  clipboardSyncEnabled.set(true);
  writeText = vi.fn().mockResolvedValue(undefined);
  readText = vi.fn().mockResolvedValue('');
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText, readText },
    configurable: true,
  });
});

afterEach(() => {
  bridge?.destroy();
  bridge = null;
});

describe('ClipboardBridge remote -> device', () => {
  it('writes remote clipboard text to the device clipboard', async () => {
    const { bridge } = setup();
    await bridge.onRemoteClipboard('hello');
    expect(writeText).toHaveBeenCalledWith('hello');
  });

  it('dedupes identical remote values', async () => {
    const { bridge } = setup();
    await bridge.onRemoteClipboard('hello');
    await bridge.onRemoteClipboard('hello');
    expect(writeText).toHaveBeenCalledTimes(1);
  });

  it('defers a write that needs a gesture and flushes it on the next gesture', async () => {
    const { bridge } = setup();
    writeText.mockRejectedValueOnce(new Error('needs user gesture'));
    await bridge.onRemoteClipboard('deferred');
    expect(writeText).toHaveBeenCalledTimes(1); // the failed attempt

    await bridge.onUserGesture();
    expect(writeText).toHaveBeenLastCalledWith('deferred'); // flushed
  });
});

describe('ClipboardBridge device -> remote', () => {
  it('reads the device clipboard on a gesture and forwards changes', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('world');
    await bridge.onUserGesture();
    expect(sent).toContainEqual({ type: 'clipboard', text: 'world' });
  });

  it('reads only once per focus session until re-armed', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('first');
    await bridge.onUserGesture();
    readText.mockResolvedValue('second');
    await bridge.onUserGesture(); // not re-armed -> no read
    expect(sent).toEqual([{ type: 'clipboard', text: 'first' }]);

    window.dispatchEvent(new Event('focus')); // re-arm
    await bridge.onUserGesture();
    expect(sent).toContainEqual({ type: 'clipboard', text: 'second' });
  });

  it('does not echo a value it just received from the remote', async () => {
    const { sent, bridge } = setup();
    await bridge.onRemoteClipboard('shared');
    readText.mockResolvedValue('shared'); // device now holds the remote value
    await bridge.onUserGesture();
    expect(sent).toEqual([]); // dedup prevents the loop
  });
});

describe('ClipboardBridge disabled', () => {
  it('does nothing when sync is off', async () => {
    const { sent, bridge } = setup();
    clipboardSyncEnabled.set(false);
    readText.mockResolvedValue('x');
    await bridge.onRemoteClipboard('y');
    await bridge.onUserGesture();
    expect(writeText).not.toHaveBeenCalled();
    expect(sent).toEqual([]);
  });
});
