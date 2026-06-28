// Bridges the device clipboard (browser Clipboard API) with the remote
// (nested compositor) clipboard, which the Rust server exposes over the
// `/client` WebSocket as `clipboard` messages (see src/clipboard.rs).
//
// Browser permission realities shape the design:
//  - writeText/write (remote -> device) works without a prompt on Chrome while
//    the tab is focused; Firefox/Safari require a user gesture, so a remote
//    change is written immediately if possible, else stashed and flushed on the
//    next gesture (onUserGesture).
//  - device -> remote needs a clipboard read, which is permission-gated. Never
//    on a plain mouse click -- that pops the Firefox/Safari "Paste" affordance
//    and hijacks clicks. Triggers:
//      * the browser `paste` event (Ctrl+V), whose clipboardData reads with NO
//        prompt (desktop).
//      * the first *touch* on the stream after the tab regains focus (read()
//        shows at most a one-time permission prompt on Chrome) -- mobile.
//      * on tab focus/visibility, a proactive read WHEN clipboard-read is
//        already granted (Chrome), so the remote clipboard stays current and
//        right-click → Paste inside the remote works, not just Ctrl+V.
import type { ClientMessage } from './protocol';
import { persisted } from './persisted';

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
  /// Whether to read the device clipboard on the next *touch*. Armed when the
  /// tab (re)gains focus so we read at most once per focus session, not on
  /// every tap.
  private armed = true;

  private readonly unsub: () => void;
  private readonly onFocus = () => {
    this.armed = true;
    void this.maybeProactiveRead();
  };
  private readonly onVisibility = () => {
    if (document.visibilityState === 'visible') {
      this.armed = true;
      void this.maybeProactiveRead();
    }
  };
  /// device -> remote: the user pasted (Ctrl+V). clipboardData is readable
  /// here without any permission prompt.
  private readonly onPaste = (e: ClipboardEvent): void => {
    if (!this.enabled || !e.clipboardData) return;
    // Prefer an image; fall back to text.
    for (const item of e.clipboardData.items) {
      if (item.kind === 'file' && item.type === 'image/png') {
        const file = item.getAsFile();
        if (file) {
          void file.arrayBuffer().then((buf) => this.forwardImage('image/png', new Uint8Array(buf)));
          return;
        }
      }
    }
    const text = e.clipboardData.getData('text/plain');
    if (text) this.forwardText(text);
  };

  constructor(send: Send, sendImage: SendImage) {
    this.send = send;
    this.sendImage = sendImage;
    this.unsub = clipboardSyncEnabled.subscribe((v) => {
      this.enabled = v;
    });
    window.addEventListener('paste', this.onPaste);
    window.addEventListener('focus', this.onFocus);
    document.addEventListener('visibilitychange', this.onVisibility);
  }

  destroy(): void {
    this.unsub();
    window.removeEventListener('paste', this.onPaste);
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

  /// Call from a gesture handler. Always flushes a deferred remote->device
  /// write. Reads the device clipboard (device->remote) ONLY when `fromTouch`
  /// and armed -- never on mouse/keyboard, since a clipboard read on a mouse
  /// click pops the Firefox/Safari "Paste" affordance and hijacks the click.
  /// Desktop uses the `paste` event (Ctrl+V) for device->remote instead.
  async onUserGesture(fromTouch = false): Promise<void> {
    if (!this.enabled) return;
    // Read FIRST: a deferred write (writeDevice) would consume this gesture's
    // transient activation and make the subsequent clipboard read fail.
    if (fromTouch && this.armed) {
      this.armed = false;
      await this.readDevice();
    }
    if (this.pendingWrite !== null) {
      const p = this.pendingWrite;
      this.pendingWrite = null;
      await this.writeDevice(p);
    }
  }

  /// Keeps the remote clipboard in sync without a gesture, so ANY paste method
  /// in the remote (Ctrl+V, right-click → Paste, middle-click) sees the device
  /// clipboard -- not just a browser Ctrl+V. Reads only when `clipboard-read`
  /// is already granted (Chrome): elsewhere a gesture-less read pops the
  /// "Paste" affordance, so we skip and rely on the paste event / touch read.
  private async maybeProactiveRead(): Promise<void> {
    if (!this.enabled) return;
    try {
      const perm = await navigator.permissions.query({ name: 'clipboard-read' as PermissionName });
      if (perm.state !== 'granted') return;
    } catch {
      return; // permissions API has no clipboard-read here (Firefox/Safari)
    }
    await this.readDevice();
  }

  /// device -> remote: read the device clipboard (image preferred, else text)
  /// and forward it. Called from a touch gesture and from maybeProactiveRead.
  private async readDevice(): Promise<void> {
    try {
      const items = await navigator.clipboard.read();
      for (const item of items) {
        if (item.types.includes('image/png')) {
          const bytes = new Uint8Array(await (await item.getType('image/png')).arrayBuffer());
          this.forwardImage('image/png', bytes);
          return;
        }
      }
      for (const item of items) {
        if (item.types.includes('text/plain')) {
          this.forwardText(await (await item.getType('text/plain')).text());
          return;
        }
      }
    } catch {
      // Fall back to readText() on browsers without read(); ignore failures
      // (no permission / not focused / unsupported).
      try {
        this.forwardText(await navigator.clipboard.readText());
      } catch {
        /* ignore */
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

  private forwardImage(mime: string, bytes: Uint8Array): void {
    const key = imageKey(mime, bytes);
    if (key !== this.lastKey) {
      this.lastKey = key;
      this.sendImage(mime, bytes);
    }
  }
}
