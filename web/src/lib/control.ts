// Owns the `/ws` control channel: connect, queue sends until OPEN, send
// `{type:"ready"}` on open, and reflect connection state into stats.ts for
// StatsPanel.svelte. Auto-reconnect is still deferred -- a `closed`/`error`
// state here is terminal until the page is reloaded.
import type { ClientMessage } from './protocol';
import { setConnectionState } from './stats';

export class ControlChannel {
  private ws: WebSocket | null = null;
  private sendQueue: string[] = [];

  connect(): void {
    setConnectionState('connecting');
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/ws`;

    const ws = new WebSocket(url);
    ws.onopen = () => {
      setConnectionState('open');
      this.send({ type: 'ready' });
      const queued = this.sendQueue;
      this.sendQueue = [];
      for (const json of queued) {
        ws.send(json);
      }
    };
    ws.onerror = (e) => {
      setConnectionState('error');
      console.error('Control WebSocket error:', e);
    };
    ws.onclose = () => setConnectionState('closed');
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
