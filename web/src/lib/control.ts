// Owns the `/ws` control channel: connect, queue sends until OPEN, send
// `{type:"ready"}` on open, reflect connection state into stats.ts for
// StatsPanel.svelte, and auto-reconnect with backoff on an unexpected
// close.
import { nextBackoffDelayMs } from './backoff';
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

  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  // Distinguishes an intentional `close()` (e.g. page teardown) from the
  // socket closing on its own -- only the latter should trigger a
  // reconnect.
  private closedByCaller = false;

  constructor(opts: ControlChannelOptions = {}) {
    this.onCodec = opts.onCodec;
  }

  connect(): void {
    this.closedByCaller = false;
    this.openSocket();
  }

  private openSocket(): void {
    setConnectionState('connecting');
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/ws`;

    const ws = new WebSocket(url);
    ws.onopen = () => {
      this.reconnectAttempt = 0;
      setConnectionState('open');
      this.send({ type: 'ready' });
      const queued = this.sendQueue;
      this.sendQueue = [];
      for (const json of queued) {
        ws.send(json);
      }
    };
    ws.onerror = (e) => {
      console.error('Control WebSocket error:', e);
    };
    // `onerror` always precedes `onclose` for a failed/dropped connection
    // (per the WebSocket spec), so reconnect scheduling lives only here --
    // otherwise a single drop would queue two attempts.
    ws.onclose = () => this.scheduleReconnect();
    ws.onmessage = (event) => this.onServerMessage(event.data as string);
    this.ws = ws;
  }

  private scheduleReconnect(): void {
    if (this.closedByCaller) {
      setConnectionState('closed');
      return;
    }
    setConnectionState('reconnecting');
    const delay = nextBackoffDelayMs(this.reconnectAttempt);
    this.reconnectAttempt += 1;
    console.info(`Control WebSocket closed, reconnecting in ${Math.round(delay)}ms`);
    this.reconnectTimer = setTimeout(() => this.openSocket(), delay);
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
    this.closedByCaller = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
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
