// Mirrors the wire protocol implemented by the Rust server. Keep this file
// in sync with the source of truth: src/server.rs (SignalingMessage /
// ServerMessage), src/proto.rs (header constants), src/input/* (TouchEvent /
// MouseEvent / KeyboardEvent), and the per-message payload layouts documented
// next to `encode_unified_video_frame` / `encode_unified_audio_frame` /
// `encode_unified_control` in src/server.rs.

export interface TouchPoint {
  identifier: number;
  x: number;
  y: number;
  pressure: number;
}

// Named `*Message`, not `TouchEvent`/`PointerEvent`, to avoid shadowing the
// DOM's own ambient `TouchEvent`/`PointerEvent` types that input.ts needs.
export type TouchMessage =
  | { eventType: 'touchstart'; touches: TouchPoint[] }
  | { eventType: 'touchmove'; touches: TouchPoint[] }
  | { eventType: 'touchend'; touches: TouchPoint[] }
  | { eventType: 'touchcancel'; touches: TouchPoint[] };

export interface PointerPoint {
  x: number;
  y: number;
  button: number;
  pointerType: string;
  pressure: number;
}

export type PointerMessage =
  | { eventType: 'pointerdown'; pointer: PointerPoint }
  | { eventType: 'pointermove'; pointer: PointerPoint }
  | { eventType: 'pointerup'; pointer: PointerPoint }
  | { eventType: 'pointercancel'; pointer: PointerPoint }
  | { eventType: 'wheel'; x: number; y: number; deltaX: number; deltaY: number };

// `code` is `KeyboardEvent.code` -- the physical, layout-independent key
// identifier (e.g. "KeyA", "ShiftLeft") -- never `KeyboardEvent.key`, which
// is the layout-resolved character. The server's own XKB keymap resolves
// the resulting keysym from this physical key, the same way real hardware
// does (see src/input/keyboard.rs).
export type KeyMessage =
  | { eventType: 'keydown'; code: string }
  | { eventType: 'keyup'; code: string };

/// Client→server messages, sent over the `/client` unified WebSocket inside
/// a `MSG_CLIENT_MSG` frame whose payload is the JSON encoding of this type.
/// Mirrors `SignalingMessage` in src/server.rs.
export type ClientMessage =
  | { type: 'ready' }
  | { type: 'resize'; width: number; height: number }
  | ({ type: 'touch' } & TouchMessage)
  | ({ type: 'pointer' } & PointerMessage)
  | ({ type: 'key' } & KeyMessage)
  | { type: 'request_keyframe' }
  | { type: 'ping'; client_ts: number }
  | {
      type: 'latency';
      encoding_ms?: number;
      network_ms?: number;
      jitter_buffer_ms?: number;
      decoding_ms?: number;
      total_ms: number;
      // Count of /client frame arrivals within ~3ms of the previous one
      // this window -- see VideoStream.flushDiagnostics and the server's
      // SignalingMessage::Latency::burst_count doc.
      burst_count?: number;
      // Average wall-clock cost of ctx.drawImage(VideoFrame) alone, isolated
      // from the decode itself by stamping performance.now() before and after
      // the call. On Firefox this path can be a GPU→CPU→GPU round-trip.
      blit_ms?: number;
    };

/// Cursor state pushed from the compositor. The browser uses this to render
/// a client-side cursor overlay on top of the video canvas.
export type CursorUpdate =
  | { kind: 'default' }
  | { kind: 'hidden' }
  | { kind: 'named'; name: string }
  | {
      kind: 'surface';
      width: number;
      height: number;
      hotspot_x: number;
      hotspot_y: number;
      /** Base64-encoded RGBA pixel data (width × height × 4 bytes). */
      rgba: string;
    };

/// Server→client control messages, sent inside a `MSG_CONTROL` frame whose
/// payload is the JSON encoding of this type. Mirrors `ServerMessage` in
/// src/server.rs.
export type ServerMessage =
  | { type: 'bitrate'; bps: number }
  | { type: 'codec'; codec: string }
  | { type: 'cursor'; cursor: CursorUpdate };

// ─── Unified binary protocol framing ─────────────────────────────────────────
//
// Every WebSocket message on `/client` is a single framed message:
//
//   byte 0      : msg_type (u8)
//   byte 1      : flags    (u8; meaning depends on msg_type)
//   bytes 2-3   : reserved (u16, always 0)
//   bytes 4-7   : payload_len (u32, little-endian)
//   bytes 8..   : payload   (payload_len bytes)
//
// Mirrors src/proto.rs. Constants must stay in sync with the server.

export const HEADER_LEN = 8;

export const MSG_VIDEO_FRAME = 0x01;
export const MSG_AUDIO_FRAME = 0x02;
export const MSG_CONTROL = 0x03;
export const MSG_CLIENT_MSG = 0x10;

export const FLAG_KEYFRAME = 0b0000_0001;
export const FLAG_HAS_PING = 0b0000_0010;

export interface UnifiedHeader {
  msgType: number;
  flags: number;
  payloadLen: number;
}

export function parseUnifiedHeader(buf: ArrayBuffer): UnifiedHeader {
  if (buf.byteLength < HEADER_LEN) {
    throw new Error(`unified frame too short: ${buf.byteLength} bytes (need >= ${HEADER_LEN})`);
  }
  const view = new DataView(buf);
  return {
    msgType: view.getUint8(0),
    flags: view.getUint8(1),
    payloadLen: view.getUint32(4, true),
  };
}

/// Encodes a complete framed message into a freshly-allocated ArrayBuffer.
/// Layout must match `proto::encode_msg` in src/proto.rs byte-for-byte, so a
/// server built against either side parses the other's output identically.
export function encodeUnifiedFrame(msgType: number, flags: number, payload: ArrayBuffer | Uint8Array): ArrayBuffer {
  const payloadBytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
  const buf = new ArrayBuffer(HEADER_LEN + payloadBytes.byteLength);
  const view = new DataView(buf);
  view.setUint8(0, msgType);
  view.setUint8(1, flags);
  view.setUint16(2, 0, true); // reserved
  view.setUint32(4, payloadBytes.byteLength, true);
  new Uint8Array(buf, HEADER_LEN).set(payloadBytes);
  return buf;
}

export function encodeClientMessage(msg: ClientMessage): ArrayBuffer {
  // TextEncoder is available in browsers and (the relevant subset of) Node
  // test environments; the JSON payload is what the server's
  // `serde_json::from_slice::<SignalingMessage>` expects.
  const json = new TextEncoder().encode(JSON.stringify(msg));
  return encodeUnifiedFrame(MSG_CLIENT_MSG, 0, json);
}

/// Decoded payload of a `MSG_VIDEO_FRAME` from the `/client` endpoint.
/// Layout (after the 8-byte proto header, big-endian unless noted):
///   bytes 0-3   : frame_id (u32 BE)
///   bytes 4-11  : ping_echo_client_ts (f64 BE; 0.0 when flags & FLAG_HAS_PING == 0)
///   bytes 12-19 : capture_to_encode_ms (f64 BE)
///   bytes 20..  : raw Annex-B H.264 NAL data
export interface VideoFramePayload {
  isKeyframe: boolean;
  frameId: number;
  pingEchoClientTs: number | null;
  captureToEncodeMs: number;
  /** Annex-B H.264 data starting from offset 20 of the framed payload. */
  data: Uint8Array;
}

export function parseVideoFramePayload(payload: ArrayBuffer, flags: number): VideoFramePayload {
  if (payload.byteLength < 20) {
    throw new Error(`MSG_VIDEO_FRAME payload too short: ${payload.byteLength} (need >= 20)`);
  }
  const view = new DataView(payload);
  const isKeyframe = (flags & FLAG_KEYFRAME) !== 0;
  const hasPing = (flags & FLAG_HAS_PING) !== 0;
  const frameId = view.getUint32(0, false);
  const pingRaw = view.getFloat64(4, false);
  const captureToEncodeMs = view.getFloat64(12, false);
  return {
    isKeyframe,
    frameId,
    pingEchoClientTs: hasPing ? pingRaw : null,
    captureToEncodeMs,
    data: new Uint8Array(payload, 20, payload.byteLength - 20),
  };
}

/// Decoded payload of a `MSG_AUDIO_FRAME` from the `/client` endpoint.
/// Layout (after the 8-byte proto header):
///   bytes 0-7  : pts_us (u64 BE)
///   bytes 8..  : raw Opus packet
export interface AudioFramePayload {
  ptsUs: number;
  data: Uint8Array;
}

export function parseAudioFramePayload(payload: ArrayBuffer): AudioFramePayload {
  if (payload.byteLength < 8) {
    throw new Error(`MSG_AUDIO_FRAME payload too short: ${payload.byteLength} (need >= 8)`);
  }
  const view = new DataView(payload);
  // JS numbers can exactly represent integers up to 2^53; PTS values for
  // audio (microseconds from stream start) stay well within that range.
  const high = view.getUint32(0, false);
  const low = view.getUint32(4, false);
  return {
    ptsUs: high * 2 ** 32 + low,
    data: new Uint8Array(payload, 8, payload.byteLength - 8),
  };
}

/// Baseline profile, Annex-B, SPS/PPS repeated inline on keyframes -- no
/// out-of-band `description` is needed. This is just a startup default for
/// before the server's first `codec` message arrives (see ServerMessage):
/// the server picks the actual H.264 level from resolution/framerate (see
/// `select_h264_level` in src/encoder/mod.rs) and can change it later if the
/// resolution changes, which VideoStream applies via `setCodec`.
export const DECODER_CONFIG: VideoDecoderConfig = {
  codec: 'avc1.42E01F',
  optimizeForLatency: true,
};