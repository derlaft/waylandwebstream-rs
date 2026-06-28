import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ClipboardBridge, clipboardSyncEnabled } from './clipboard';
import type { ClientMessage } from './protocol';

let writeText: ReturnType<typeof vi.fn>;
let write: ReturnType<typeof vi.fn>;
let readText: ReturnType<typeof vi.fn>;
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
  };
}

// Fake clipboard.read() items.
function imageReadItem(bytes: Uint8Array) {
  return { types: ['image/png'], getType: async () => ({ arrayBuffer: async () => bytes.buffer }) };
}
function textReadItem(text: string) {
  return { types: ['text/plain'], getType: async () => ({ text: async () => text }) };
}

// Synthetic `paste` events (jsdom has no ClipboardEvent ctor with clipboardData).
function firePasteText(text: string): void {
  const e = new Event('paste') as Event & { clipboardData: unknown };
  e.clipboardData = { items: [], getData: (t: string) => (t === 'text/plain' ? text : '') };
  window.dispatchEvent(e);
}
function firePasteImage(bytes: Uint8Array): void {
  const e = new Event('paste') as Event & { clipboardData: unknown };
  e.clipboardData = {
    items: [{ kind: 'file', type: 'image/png', getAsFile: () => ({ arrayBuffer: async () => bytes.buffer }) }],
    getData: () => '',
  };
  window.dispatchEvent(e);
}
const flush = () => new Promise((r) => setTimeout(r, 0));

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
  write = vi.fn().mockResolvedValue(undefined);
  readText = vi.fn().mockResolvedValue('');
  read = vi.fn().mockResolvedValue(undefined); // default -> fallback to readText
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText, write, readText, read },
    configurable: true,
  });
});

afterEach(() => {
  bridge?.destroy();
  bridge = null;
});

describe('ClipboardBridge remote -> device (write)', () => {
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
    expect(writeText).toHaveBeenCalledTimes(1);
    await bridge.onUserGesture(); // flush (mouse/keyboard gesture)
    expect(writeText).toHaveBeenLastCalledWith('deferred');
  });

  it('writes a remote image to the device clipboard', async () => {
    const { bridge } = setup();
    await bridge.onRemoteImage('image/png', new Uint8Array([1, 2, 3]));
    expect(write).toHaveBeenCalledTimes(1);
  });
});

describe('ClipboardBridge device -> remote (touch read)', () => {
  it('reads the device clipboard on a TOUCH gesture and forwards text', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('world');
    await bridge.onUserGesture(true);
    expect(sent).toContainEqual({ type: 'clipboard', text: 'world' });
  });

  it('reads an image on a touch gesture (preferred over text)', async () => {
    const { images, sent, bridge } = setup();
    const bytes = new Uint8Array([9, 8, 7]);
    read.mockResolvedValue([imageReadItem(bytes)]);
    await bridge.onUserGesture(true);
    expect(images).toEqual([{ mime: 'image/png', bytes }]);
    expect(sent).toEqual([]);
  });

  it('does NOT read on a mouse/keyboard gesture (no Paste prompt)', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('x');
    read.mockResolvedValue([textReadItem('x')]);
    await bridge.onUserGesture(false);
    expect(read).not.toHaveBeenCalled();
    expect(readText).not.toHaveBeenCalled();
    expect(sent).toEqual([]);
  });

  it('reads only once per focus session until re-armed', async () => {
    const { sent, bridge } = setup();
    readText.mockResolvedValue('first');
    await bridge.onUserGesture(true);
    readText.mockResolvedValue('second');
    await bridge.onUserGesture(true); // not re-armed -> no read
    expect(sent).toEqual([{ type: 'clipboard', text: 'first' }]);

    window.dispatchEvent(new Event('focus')); // re-arm
    await bridge.onUserGesture(true);
    expect(sent).toContainEqual({ type: 'clipboard', text: 'second' });
  });
});

describe('ClipboardBridge device -> remote (paste event)', () => {
  it('forwards pasted text to the remote', () => {
    const { sent } = setup();
    firePasteText('viapaste');
    expect(sent).toContainEqual({ type: 'clipboard', text: 'viapaste' });
  });

  it('forwards a pasted image to the remote', async () => {
    const { images } = setup();
    firePasteImage(new Uint8Array([5, 5, 5]));
    await flush();
    expect(images).toEqual([{ mime: 'image/png', bytes: new Uint8Array([5, 5, 5]) }]);
  });

  it('does not echo a value it just received from the remote', async () => {
    const { sent, bridge } = setup();
    await bridge.onRemoteClipboard('shared');
    firePasteText('shared');
    expect(sent).toEqual([]); // dedup prevents the loop
  });
});

describe('ClipboardBridge disabled', () => {
  it('does nothing when sync is off', async () => {
    const { sent, images, bridge } = setup();
    clipboardSyncEnabled.set(false);
    readText.mockResolvedValue('x');
    await bridge.onRemoteClipboard('y');
    await bridge.onUserGesture(true);
    firePasteText('z');
    await flush();
    expect(writeText).not.toHaveBeenCalled();
    expect(read).not.toHaveBeenCalled();
    expect(readText).not.toHaveBeenCalled();
    expect(sent).toEqual([]);
    expect(images).toEqual([]);
  });
});
