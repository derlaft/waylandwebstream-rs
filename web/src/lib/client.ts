// Owns the `/client` unified WebSocket. Combines the legacy /ws + /stream +
// /audio endpoints into one connection using the 8-byte proto framing
// (see src/proto.rs and lib/protocol.ts):
//
//   - sends ClientMessage as MSG_CLIENT_MSG frames (JSON payload)
//   - on inbound: dispatches MSG_VIDEO_FRAME -> onVideoFrame,
//                 MSG_AUDIO_FRAME -> onAudioFrame,
//                 MSG_CONTROL     -> onCodec/onCursor/onBitrate
//
// Reconnect behavior: the server allows only one client at a time and kicks
// the previous one when a new connection arrives, so automatic reconnection
// is deliberately disabled -- a dropped or kicked connection stays closed
// until the user explicitly reconnects (Stage wires canvas click/tap/keydown
// to reconnect()). close() (intentional teardown) never reconnects either.
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
  /// Called when the remote (nested compositor) clipboard changes. The caller
  /// writes the text to the device clipboard (see lib/clipboard.ts).
  onClipboard?: (text: string) => void;
  /// Called for every decoded `MSG_VIDEO_FRAME` payload (frame_id,
  /// is_keyframe, ping echo, and the raw H.264 data). The caller feeds the
  /// H.264 to a VideoDecoder.
  onVideoFrame?: (frame: VideoFramePayload) => void;
  /// Called for every `MSG_AUDIO_FRAME` payload (pts_us + raw Opus bytes).
  /// The caller feeds the Opus to an AudioDecoder.
  onAudioFrame?: (frame: AudioFramePayload) => void;
  /// Called when the socket closes unexpectedly (dropped or kicked by the
  /// server because another client connected), not on an intentional
  /// close(). Auto-reconnect is disabled, so the caller uses this to prompt
  /// the user to reconnect (e.g. show an overlay) and to arm reconnect() on
  /// the next canvas interaction.
  onClosed?: () => void;
}

export class ClientChannel {
  private ws: WebSocket | null = null;
  // ClientMessages queued while the socket is not yet OPEN, so input events
  // emitted in the first few ms after connect() aren't dropped (e.g. an
  // immediate Resize from the viewport, or a Ready handshake).
  private sendQueue: ArrayBuffer[] = [];
  private readonly onCodec?: (codec: string) => void;
  private readonly onCursor?: (cursor: CursorUpdate) => void;
  private readonly onClipboard?: (text: string) => void;
  private readonly onVideoFrame?: (frame: VideoFramePayload) => void;
  private readonly onAudioFrame?: (frame: AudioFramePayload) => void;
  private readonly onClosed?: () => void;

  // Connection lifecycle:
  //   'idle'       — constructed, connect() not yet called. Sends are buffered
  //                  (the initial ready/resize emitted before connect()).
  //   'connecting' — socket opening; sends are buffered until OPEN.
  //   'open'       — socket OPEN; sends go out immediately.
  //   'closed'     — socket closed (dropped, kicked, or intentional). Sends
  //                  are dropped; reconnect() re-opens unless closedByCaller.
  private phase: 'idle' | 'connecting' | 'open' | 'closed' = 'idle';
  // Distinguishes an intentional `close()` (e.g. page teardown) from the
  // socket closing on its own -- only the latter arms reconnect().
  private closedByCaller = false;

  constructor(opts: ClientChannelOptions = {}) {
    this.onCodec = opts.onCodec;
    this.onCursor = opts.onCursor;
    this.onClipboard = opts.onClipboard;
    this.onVideoFrame = opts.onVideoFrame;
    this.onAudioFrame = opts.onAudioFrame;
    this.onClosed = opts.onClosed;
  }

  connect(): void {
    this.closedByCaller = false;
    this.openSocket();
  }

  /// Re-open the connection after an unexpected close. No-op if a socket is
  /// already connecting/open, or if the channel was closed intentionally via
  /// close(). Stage calls this on the first canvas interaction (click, tap,
  /// or keypress) after a disconnect -- reconnecting kicks whatever other
  /// client is currently attached, since the server allows only one.
  reconnect(): void {
    if (this.phase !== 'closed' || this.closedByCaller) return;
    this.openSocket();
  }

  private openSocket(): void {
    this.phase = 'connecting';
    setConnectionState('connecting');
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/client`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      this.phase = 'open';
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
    // (per the WebSocket spec), so close handling lives only here.
    ws.onclose = () => this.handleClose();
    ws.onmessage = (event) => this.onUnifiedFrame(event.data as ArrayBuffer);
    this.ws = ws;
  }

  private handleClose(): void {
    this.ws = null;
    this.phase = 'closed';
    // Queued sends are stale once the socket is gone; clear them so a later
    // reconnect doesn't replay input from before the disconnect.
    this.sendQueue = [];
    setConnectionState('closed');
    // Auto-reconnect is disabled. Only notify on an unexpected close so the
    // caller can prompt the user; an intentional close() is silent.
    if (!this.closedByCaller) {
      this.onClosed?.();
    }
  }

  send(msg: ClientMessage): void {
    const framed = encodeClientMessage(msg);
    if (this.phase === 'open' && this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(framed);
    } else if (this.phase === 'idle' || this.phase === 'connecting') {
      // Buffer until the socket opens (e.g. the initial ready/resize emitted
      // before connect(), or input in the first ms after connect()).
      this.sendQueue.push(framed);
    }
    // phase === 'closed': drop. There's no socket and we won't reconnect
    // until the user interacts, by which point this input is stale.
  }

  close(): void {
    this.closedByCaller = true;
    this.phase = 'closed';
    this.sendQueue = [];
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
    } else if (msg.type === 'clipboard') {
      this.onClipboard?.(msg.text);
    }
  }
}