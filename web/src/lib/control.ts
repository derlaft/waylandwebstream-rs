// Owns the `/ws` control channel: connect, queue sends until OPEN, send
// `{type:"ready"}` on open, and reflect connection state into stats.ts for
// StatsPanel.svelte. Auto-reconnect is still deferred -- a `closed`/`error`
// state here is terminal until the page is reloaded.
import type { ClientMessage, ServerMessage } from './protocol';
import { setBitrate, setConnectionState } from './stats';

export interface ControlChannelOptions {
  /// Called whenever the server pushes a new WebCodecs codec string (see
  /// ServerMessage), e.g. because a resolution change picked a different
  /// H.264 level. Lets the caller reconfigure its VideoDecoder to match.
  onCodec?: (codec: string) => void;
}

export class ControlChannel {
  private ws: WebSocket | null = null;
  private sendQueue: string[] = [];
  private readonly onCodec?: (codec: string) => void;

  constructor(opts: ControlChannelOptions = {}) {
    this.onCodec = opts.onCodec;
  }

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
    ws.onmessage = (event) => this.onServerMessage(event.data as string);
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

  private onServerMessage(data: string): void {
    let msg: ServerMessage;
    try {
      msg = JSON.parse(data);
    } catch (e) {
      console.error('Failed to parse control message:', e);
      return;
    }
    if (msg.type === 'bitrate') {
      setBitrate(msg.bps);
    } else if (msg.type === 'codec') {
      this.onCodec?.(msg.codec);
    }
  }
}
