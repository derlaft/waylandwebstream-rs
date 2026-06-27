// Owns the `/client` unified WebSocket. Combines the legacy /ws + /stream +
// /audio endpoints into one connection using the 8-byte proto framing
// (see src/proto.rs and lib/protocol.ts):
//
//   - sends ClientMessage as MSG_CLIENT_MSG frames (JSON payload)
//   - on inbound: dispatches MSG_VIDEO_FRAME -> onVideoFrame,
//                 MSG_AUDIO_FRAME -> onAudioFrame,
//                 MSG_CONTROL     -> onCodec/onCursor/onBitrate
//
// Reconnect behavior mirrors the old ControlChannel: exponential back-off
// with full jitter (lib/backoff.ts), full state reset on reconnect, no
// reconnect when close() is called intentionally.
import { nextBackoffDelayMs } from './backoff';
import {
  MSG_AUDIO_FRAME,
  MSG_CONTROL,
  MSG_VIDEO_FRAME,
  encodeClientMessage,
  parseAudioFramePayload,
  parseUnifiedHeader,
  parseVideoFramePayload,
  type AudioFramePayload,
  type ClientMessage,
  type CursorUpdate,
  type ServerMessage,
  type VideoFramePayload,
} from './protocol';
import { setBitrate, setConnectionState } from './stats';

export interface ClientChannelOptions {
  /// Called whenever the server pushes a new WebCodecs codec string (see
  /// ServerMessage), e.g. because a resolution change picked a different
  /// H.264 level. The caller reconfigures its VideoDecoder to match.
  onCodec?: (codec: string) => void;
  /// Called whenever the compositor changes the cursor (shape, hotspot, or
  /// visibility). The caller applies it to the canvas's CSS cursor property.
  onCursor?: (cursor: CursorUpdate) => void;
  /// Called for every decoded `MSG_VIDEO_FRAME` payload (frame_id,
  /// is_keyframe, ping echo, and the raw H.264 data). The caller feeds the
  /// H.264 to a VideoDecoder.
  onVideoFrame?: (frame: VideoFramePayload) => void;
  /// Called for every `MSG_AUDIO_FRAME` payload (pts_us + raw Opus bytes).
  /// The caller feeds the Opus to an AudioDecoder.
  onAudioFrame?: (frame: AudioFramePayload) => void;
}

export class ClientChannel {
  private ws: WebSocket | null = null;
  // ClientMessages queued while the socket is not yet OPEN, so input events
  // emitted in the first few ms after connect() aren't dropped (e.g. an
  // immediate Resize from the viewport, or a Ready handshake).
  private sendQueue: ArrayBuffer[] = [];
  private readonly onCodec?: (codec: string) => void;
  private readonly onCursor?: (cursor: CursorUpdate) => void;
  private readonly onVideoFrame?: (frame: VideoFramePayload) => void;
  private readonly onAudioFrame?: (frame: AudioFramePayload) => void;

  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  // Distinguishes an intentional `close()` (e.g. page teardown) from the
  // socket closing on its own -- only the latter should trigger a
  // reconnect.
  private closedByCaller = false;

  constructor(opts: ClientChannelOptions = {}) {
    this.onCodec = opts.onCodec;
    this.onCursor = opts.onCursor;
    this.onVideoFrame = opts.onVideoFrame;
    this.onAudioFrame = opts.onAudioFrame;
  }

  connect(): void {
    this.closedByCaller = false;
    this.openSocket();
  }

  private openSocket(): void {
    setConnectionState('connecting');
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/client`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      this.reconnectAttempt = 0;
      setConnectionState('open');
      this.send({ type: 'ready' });
      const queued = this.sendQueue;
      this.sendQueue = [];
      for (const frame of queued) {
        ws.send(frame);
      }
    };
    ws.onerror = (e) => {
      console.error('Client WebSocket error:', e);
    };
    // `onerror` always precedes `onclose` for a failed/dropped connection
    // (per the WebSocket spec), so reconnect scheduling lives only here --
    // otherwise a single drop would queue two attempts.
    ws.onclose = () => this.scheduleReconnect();
    ws.onmessage = (event) => this.onUnifiedFrame(event.data as ArrayBuffer);
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
    console.info(`Client WebSocket closed, reconnecting in ${Math.round(delay)}ms`);
    this.reconnectTimer = setTimeout(() => this.openSocket(), delay);
  }

  send(msg: ClientMessage): void {
    const framed = encodeClientMessage(msg);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(framed);
    } else {
      this.sendQueue.push(framed);
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

  /// Decode a single binary WebSocket message and dispatch it. A malformed
  /// (too-short, wrong-type, or oversized-payload) frame is logged and
  /// dropped -- never closes the connection, matching the server's policy
  /// of "a single bad frame can't kill the connection".
  private onUnifiedFrame(buf: ArrayBuffer): void {
    let header;
    try {
      header = parseUnifiedHeader(buf);
    } catch (e) {
      console.error('Failed to parse unified header:', e);
      return;
    }
    const totalLen = 8 + header.payloadLen;
    if (totalLen > buf.byteLength) {
      console.error(
        `Truncated unified frame: header claims ${header.payloadLen} payload bytes, have ${buf.byteLength - 8}`,
      );
      return;
    }
    const payload = buf.slice(8, totalLen);

    switch (header.msgType) {
      case MSG_VIDEO_FRAME: {
        if (!this.onVideoFrame) return;
        try {
          this.onVideoFrame(parseVideoFramePayload(payload, header.flags));
        } catch (e) {
          console.error('Failed to parse MSG_VIDEO_FRAME payload:', e);
        }
        return;
      }
      case MSG_AUDIO_FRAME: {
        if (!this.onAudioFrame) return;
        try {
          this.onAudioFrame(parseAudioFramePayload(payload));
        } catch (e) {
          console.error('Failed to parse MSG_AUDIO_FRAME payload:', e);
        }
        return;
      }
      case MSG_CONTROL: {
        this.onControlPayload(payload);
        return;
      }
      default: {
        // The server never sends CLIENT_MSG to itself; silently ignore any
        // unknown msg_type we don't handle so a future protocol addition
        // doesn't crash an old client.
        return;
      }
    }
  }

  private onControlPayload(payload: ArrayBuffer): void {
    let msg: ServerMessage;
    try {
      msg = JSON.parse(new TextDecoder().decode(new Uint8Array(payload)));
    } catch (e) {
      console.error('Failed to parse MSG_CONTROL JSON:', e);
      return;
    }
    if (msg.type === 'bitrate') {
      setBitrate(msg.bps);
    } else if (msg.type === 'codec') {
      this.onCodec?.(msg.codec);
    } else if (msg.type === 'cursor') {
      this.onCursor?.(msg.cursor);
    }
  }
}