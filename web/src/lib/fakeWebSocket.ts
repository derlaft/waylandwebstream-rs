// Minimal WebSocket stand-in for reconnect tests: no real networking, just
// enough of the API (readyState, on* handlers, send/close) for control.ts
// and stream.ts to drive, plus test-only `simulate*` hooks the test calls
// itself rather than the code under test.
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
  readonly sent: string[] = [];

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
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
}

export function installFakeWebSocket(): void {
  FakeWebSocket.instances = [];
  vi.stubGlobal('WebSocket', FakeWebSocket);
}
