import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ClipboardBridge, clipboardSyncEnabled } from './clipboard';
import type { ClientMessage } from './protocol';

let writeText: ReturnType<typeof vi.fn>;
let readText: ReturnType<typeof vi.fn>;
let write: ReturnType<typeof vi.fn>;
let read: ReturnType<typeof vi.fn>;
let bridge: ClipboardBridge | null = null;

// jsdom lacks ClipboardItem; a minimal stand-in is enough for our write path.
if (typeof (globalThis as Record<string, unknown>).ClipboardItem === 'undefined') {
  (globalThis as Record<string, unknown>).ClipboardItem = class {
    items: Record<string, Blob>;
    types: string[];
    constructor(items: Record<string, Blob>) {
      this.items = items;
      this.types = Object.keys(items);
    }
    async getType(t: string): Promise<Blob> {
      return this.items[t];
    }
  };
}

// A fake clipboard read item: { types, getType(mime) -> blob-like }.
function imageItem(bytes: Uint8Array) {
  return { types: ['image/png'], getType: async () => ({ arrayBuffer: async () => bytes.buffer }) };
}
function textItem(text: string) {
  return { types: ['text/plain'], getType: async () => ({ text: async () => text }) };
}

function setup(): {
  sent: ClientMessage[];
  images: Array<{ mime: string; bytes: Uint8Array }>;
  bridge: ClipboardBridge;
} {
  const sent: ClientMessage[] = [];
  const images: Array<{ mime: string; bytes: Uint8Array }> = [];
  bridge = new ClipboardBridge(
    (m) => sent.push(m),
    (mime, bytes) => images.push({ mime, bytes }),
  );
  return { sent, images, bridge };
}

beforeEach(() => {
  clipboardSyncEnabled.set(true);
  writeText = vi.fn().mockResolvedValue(undefined);
  readText = vi.fn().mockResolvedValue('');
  write = vi.fn().mockResolvedValue(undefined);
  // Default read() resolves undefined so the text tests fall back to readText;
  // image/text-via-read tests override it.
  read = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText, readText, write, read },
    configurable: true,
  });
});

afterEach(() => {
  bridge?.destroy();
  bridge = null;
});

describe('ClipboardBridge text remote -> device', () => {
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

describe('ClipboardBridge text device -> remote', () => {
  it('reads the device clipboard on a gesture and forwards changes', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('world'); // read() resolves undefined -> fallback
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

  it('reads text via clipboard.read() when available', async () => {
    const { sent, bridge } = setup();
    read.mockResolvedValue([textItem('via-read')]);
    await bridge.onUserGesture();
    expect(sent).toContainEqual({ type: 'clipboard', text: 'via-read' });
  });

  it('does not echo a value it just received from the remote', async () => {
    const { sent, bridge } = setup();
    await bridge.onRemoteClipboard('shared');
    readText.mockResolvedValue('shared');
    await bridge.onUserGesture();
    expect(sent).toEqual([]); // dedup prevents the loop
  });
});

describe('ClipboardBridge images', () => {
  it('writes a remote image to the device clipboard', async () => {
    const { bridge } = setup();
    await bridge.onRemoteImage('image/png', new Uint8Array([1, 2, 3]));
    expect(write).toHaveBeenCalledTimes(1);
  });

  it('dedupes identical remote images', async () => {
    const { bridge } = setup();
    await bridge.onRemoteImage('image/png', new Uint8Array([1, 2, 3]));
    await bridge.onRemoteImage('image/png', new Uint8Array([1, 2, 3]));
    expect(write).toHaveBeenCalledTimes(1);
  });

  it('reads an image from the device clipboard on a gesture (preferred over text)', async () => {
    const { images, sent, bridge } = setup();
    const bytes = new Uint8Array([9, 8, 7]);
    read.mockResolvedValue([imageItem(bytes)]);
    await bridge.onUserGesture();
    expect(images).toEqual([{ mime: 'image/png', bytes }]);
    expect(sent).toEqual([]); // image path, no text sent
  });

  it('does not echo an image it just received from the remote', async () => {
    const { images, bridge } = setup();
    const bytes = new Uint8Array([4, 5, 6]);
    await bridge.onRemoteImage('image/png', bytes);
    read.mockResolvedValue([imageItem(bytes)]);
    await bridge.onUserGesture();
    expect(images).toEqual([]); // dedup prevents the loop
  });
});

describe('ClipboardBridge disabled', () => {
  it('does nothing when sync is off', async () => {
    const { sent, images, bridge } = setup();
    clipboardSyncEnabled.set(false);
    readText.mockResolvedValue('x');
    read.mockResolvedValue([imageItem(new Uint8Array([1]))]);
    await bridge.onRemoteClipboard('y');
    await bridge.onRemoteImage('image/png', new Uint8Array([2]));
    await bridge.onUserGesture();
    expect(writeText).not.toHaveBeenCalled();
    expect(write).not.toHaveBeenCalled();
    expect(sent).toEqual([]);
    expect(images).toEqual([]);
  });
});
