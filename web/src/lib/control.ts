// Owns the `/ws` control channel: connect, queue sends until OPEN, send
// `{type:"ready"}` on open. Pulled forward from its nominal Phase 5 slot
// because Stage.svelte (Phase 4) needs a `sendControl` to hand to
// stream.ts/viewport.ts/input.ts -- the rest of Phase 5 (pushing connection
// state into stats.ts, auto-reconnect) is still deferred.
import type { ClientMessage } from './protocol';

export class ControlChannel {
  private ws: WebSocket | null = null;
  private sendQueue: string[] = [];

  connect(): void {
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/ws`;

    const ws = new WebSocket(url);
    ws.onopen = () => {
      this.send({ type: 'ready' });
      const queued = this.sendQueue;
      this.sendQueue = [];
      for (const json of queued) {
        ws.send(json);
      }
    };
    ws.onerror = (e) => console.error('Control WebSocket error:', e);
    this.ws = ws;
  }

  send(msg: ClientMessage): void {
    const json = JSON.stringify(msg);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(json);
    } else {
      this.sendQueue.push(json);
    }
  }

  close(): void {
    this.ws?.close();
    this.ws = null;
  }
}
