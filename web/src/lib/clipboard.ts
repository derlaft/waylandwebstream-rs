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

export class ClipboardBridge {
  private readonly send: Send;
  private enabled = true;
  /// Last text seen in either direction -- dedupes so a value doesn't ping-pong
  /// between device and remote.
  private lastValue = '';
  /// A remote->device value that couldn't be written yet (needed a gesture);
  /// flushed on the next user gesture.
  private pendingWrite: string | null = null;
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

  constructor(send: Send) {
    this.send = send;
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

  /// remote -> device: the nested compositor's selection changed.
  async onRemoteClipboard(text: string): Promise<void> {
    if (!this.enabled || text === this.lastValue) return;
    this.lastValue = text;
    await this.writeDevice(text);
  }

  private async writeDevice(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      this.pendingWrite = null;
    } catch {
      // Needs a user gesture (Firefox/Safari) -- defer to the next one.
      this.pendingWrite = text;
    }
  }

  /// device -> remote: call from a user-gesture handler (pointerdown/keydown).
  /// Flushes any deferred write, then -- once per focus session -- reads the
  /// device clipboard and forwards changes to the remote.
  async onUserGesture(): Promise<void> {
    if (!this.enabled) return;

    if (this.pendingWrite !== null) {
      const text = this.pendingWrite;
      this.pendingWrite = null;
      try {
        await navigator.clipboard.writeText(text);
      } catch {
        this.pendingWrite = text;
      }
    }

    if (!this.armed) return;
    this.armed = false;
    try {
      const text = await navigator.clipboard.readText();
      if (text && text !== this.lastValue) {
        this.lastValue = text;
        this.send({ type: 'clipboard', text });
      }
    } catch {
      // No permission, not focused, or unsupported -- ignore silently.
    }
  }
}
