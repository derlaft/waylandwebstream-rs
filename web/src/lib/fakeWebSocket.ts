// Minimal WebSocket stand-in for reconnect tests: no real networking, just
// enough of the API (readyState, on* handlers, send/close) for client.ts
// and the now-decoupled stream/audio modules to drive, plus test-only
// `simulate*` hooks the test calls itself rather than the code under test.
//
// Captures both text (`string`) and binary (`ArrayBuffer` / `Uint8Array`)
// sends into `sent` so tests can assert on the framed MSG_CLIENT_MSG
// payloads that the new `/client` transport produces.
import { vi } from 'vitest';

export class FakeWebSocket {
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readyState = 0;
  binaryType = '';
  onopen: (() => void) | null = null;
  onclose: ((e: { code: number; reason: string }) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  readonly sent: Array<string | ArrayBuffer | Uint8Array> = [];

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  send(data: string | ArrayBuffer | Uint8Array | Blob): void {
    // Keep just the framed-message shapes the client actually sends; tests
    // assert against them, so we narrow here rather than widening later.
    if (typeof data === 'string' || data instanceof ArrayBuffer || data instanceof Uint8Array) {
      this.sent.push(data);
    }
  }

  // Mirrors a real close enough for the code under test: synchronously
  // fires `onclose`, same as `simulateClose` below. Kept separate so tests
  // read naturally whether they're driving the socket as the app (`close`)
  // or as the server/network (`simulateClose`).
  close(): void {
    this.simulateClose();
  }

  simulateOpen(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.();
  }

  simulateClose(event: { code?: number; reason?: string } = {}): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ code: event.code ?? 1006, reason: event.reason ?? '' });
  }

  /// Convenience for tests: simulate a binary server frame.
  simulateBinaryMessage(data: ArrayBuffer): void {
    this.onmessage?.({ data });
  }
}

export function installFakeWebSocket(): void {
  FakeWebSocket.instances = [];
  vi.stubGlobal('WebSocket', FakeWebSocket);
}