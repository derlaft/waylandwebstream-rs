// Bridges the device clipboard (browser Clipboard API) with the remote
// (nested compositor) clipboard, which the Rust server exposes over the
// `/client` WebSocket as `clipboard` messages (see src/clipboard.rs).
//
// Browser permission realities shape the design:
//  - writeText (remote -> device) works without a prompt on Chrome while the
//    tab is focused, but Firefox/Safari require a user gesture. So a remote
//    change is written immediately if possible, else stashed and flushed on
//    the next gesture.
//  - readText (device -> remote) ALWAYS needs a user gesture and prompts the
//    first time. We can't poll it. So we read on the first stage interaction
//    after the tab (re)gains focus -- i.e. "you copied something elsewhere,
//    came back, and touched the stream" -> your clipboard syncs to the remote.
import { writable } from 'svelte/store';
import type { ClientMessage } from './protocol';

/// A `writable` mirrored into localStorage so the preference survives reloads.
function persisted<T>(key: string, initial: T) {
  let start = initial;
  try {
    const raw = localStorage.getItem(key);
    if (raw !== null) start = JSON.parse(raw) as T;
  } catch {
    /* storage blocked or corrupt -- use the default */
  }
  const store = writable<T>(start);
  store.subscribe((value) => {
    try {
      localStorage.setItem(key, JSON.stringify(value));
    } catch {
      /* ignore */
    }
  });
  return store;
}

/// Whether clipboard sync is active. On by default.
export const clipboardSyncEnabled = persisted('clipboard.enabled', true);

type Send = (msg: ClientMessage) => void;
type SendImage = (mime: string, bytes: Uint8Array) => void;

/// What the device clipboard currently holds, for deferred writes.
type Payload = { kind: 'text'; text: string } | { kind: 'image'; mime: string; bytes: Uint8Array };

// Dedup keys: text and image collapse to a short string so a value written to
// the device clipboard isn't read back and echoed to the remote (and vice
// versa). The image key uses a cheap FNV-1a hash of the bytes.
function textKey(text: string): string {
  return `T:${text}`;
}
function imageKey(mime: string, bytes: Uint8Array): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i++) {
    h ^= bytes[i];
    h = Math.imul(h, 0x01000193);
  }
  return `I:${mime}:${bytes.length}:${(h >>> 0).toString(16)}`;
}

export class ClipboardBridge {
  private readonly send: Send;
  private readonly sendImage: SendImage;
  private enabled = true;
  /// Dedup key of the last payload seen in either direction.
  private lastKey = '';
  /// A remote->device payload that couldn't be written yet (needed a gesture);
  /// flushed on the next user gesture.
  private pendingWrite: Payload | null = null;
  /// Whether to read the device clipboard on the next gesture. Armed whenever
  /// the tab (re)gains focus so we don't prompt/read on every single tap.
  private armed = true;

  private readonly unsub: () => void;
  private readonly onFocus = () => {
    this.armed = true;
  };
  private readonly onVisibility = () => {
    if (document.visibilityState === 'visible') this.armed = true;
  };

  constructor(send: Send, sendImage: SendImage) {
    this.send = send;
    this.sendImage = sendImage;
    this.unsub = clipboardSyncEnabled.subscribe((v) => {
      this.enabled = v;
    });
    window.addEventListener('focus', this.onFocus);
    document.addEventListener('visibilitychange', this.onVisibility);
  }

  destroy(): void {
    this.unsub();
    window.removeEventListener('focus', this.onFocus);
    document.removeEventListener('visibilitychange', this.onVisibility);
  }

  /// remote -> device: the nested compositor's text selection changed.
  async onRemoteClipboard(text: string): Promise<void> {
    const key = textKey(text);
    if (!this.enabled || key === this.lastKey) return;
    this.lastKey = key;
    await this.writeDevice({ kind: 'text', text });
  }

  /// remote -> device: the nested compositor's selection holds an image.
  async onRemoteImage(mime: string, bytes: Uint8Array): Promise<void> {
    const key = imageKey(mime, bytes);
    if (!this.enabled || key === this.lastKey) return;
    this.lastKey = key;
    await this.writeDevice({ kind: 'image', mime, bytes });
  }

  private async writeDevice(p: Payload): Promise<void> {
    try {
      if (p.kind === 'text') {
        await navigator.clipboard.writeText(p.text);
      } else {
        // Cast: TS types Uint8Array as generic over ArrayBufferLike, which the
        // BlobPart union rejects; the bytes are a plain ArrayBuffer at runtime.
        const blob = new Blob([p.bytes as unknown as BlobPart], { type: p.mime });
        await navigator.clipboard.write([new ClipboardItem({ [p.mime]: blob })]);
      }
      this.pendingWrite = null;
    } catch {
      // Needs a user gesture (Firefox/Safari) or unsupported -- defer.
      this.pendingWrite = p;
    }
  }

  /// device -> remote: call from a user-gesture handler (pointerdown/keydown).
  /// Flushes any deferred write, then -- once per focus session -- reads the
  /// device clipboard and forwards changes (image preferred) to the remote.
  async onUserGesture(): Promise<void> {
    if (!this.enabled) return;

    if (this.pendingWrite !== null) {
      const p = this.pendingWrite;
      this.pendingWrite = null;
      await this.writeDevice(p);
    }

    if (!this.armed) return;
    this.armed = false;
    await this.readDevice();
  }

  private async readDevice(): Promise<void> {
    // Prefer clipboard.read() (covers images + text); fall back to readText()
    // on browsers without read() (older Firefox).
    try {
      const items = await navigator.clipboard.read();
      for (const item of items) {
        if (item.types.includes('image/png')) {
          const blob = await item.getType('image/png');
          const bytes = new Uint8Array(await blob.arrayBuffer());
          const key = imageKey('image/png', bytes);
          if (key !== this.lastKey) {
            this.lastKey = key;
            this.sendImage('image/png', bytes);
          }
          return;
        }
      }
      for (const item of items) {
        if (item.types.includes('text/plain')) {
          const text = await (await item.getType('text/plain')).text();
          this.forwardText(text);
          return;
        }
      }
    } catch {
      try {
        this.forwardText(await navigator.clipboard.readText());
      } catch {
        // No permission, not focused, or unsupported -- ignore silently.
      }
    }
  }

  private forwardText(text: string): void {
    const key = textKey(text);
    if (text && key !== this.lastKey) {
      this.lastKey = key;
      this.send({ type: 'clipboard', text });
    }
  }
}
