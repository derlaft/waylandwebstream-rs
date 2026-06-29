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
  // The device clipboard text, to set as the remote (nested compositor)
  // selection. Sent on a user gesture after reading navigator.clipboard.
  | { type: 'clipboard'; text: string }
  | {
      type: 'latency';
      network_ms?: number;
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
  | { type: 'cursor'; cursor: CursorUpdate }
  // The remote (nested compositor) clipboard text; the browser writes it to
  // the device clipboard. Pushed on connect and whenever the remote changes.
  | { type: 'clipboard'; text: string };

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
/// Clipboard image (binary; text clipboard stays JSON in MSG_CONTROL).
/// Payload: mime_len (u16 LE) + mime (utf8) + raw image bytes.
export const MSG_CLIPBOARD_IMAGE = 0x04; // server -> client
export const MSG_CLIENT_MSG = 0x10;
export const MSG_CLIENT_CLIPBOARD_IMAGE = 0x11; // client -> server

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

/// Encodes a client→server clipboard image as a `MSG_CLIENT_CLIPBOARD_IMAGE`
/// frame. Payload: mime_len (u16 LE) + mime (utf8) + raw image bytes. Mirrors
/// `proto::encode_clipboard_image_payload`.
export function encodeClipboardImageMessage(mime: string, bytes: Uint8Array): ArrayBuffer {
  const mimeBytes = new TextEncoder().encode(mime);
  const payload = new Uint8Array(2 + mimeBytes.byteLength + bytes.byteLength);
  new DataView(payload.buffer).setUint16(0, mimeBytes.byteLength, true);
  payload.set(mimeBytes, 2);
  payload.set(bytes, 2 + mimeBytes.byteLength);
  return encodeUnifiedFrame(MSG_CLIENT_CLIPBOARD_IMAGE, 0, payload);
}

/// Parses a `MSG_CLIPBOARD_IMAGE` payload (the bytes after the 8-byte header)
/// into its mime type and raw image bytes.
export function parseClipboardImage(payload: ArrayBuffer): { mime: string; bytes: Uint8Array } {
  if (payload.byteLength < 2) throw new Error('clipboard image payload too short');
  const view = new DataView(payload);
  const mimeLen = view.getUint16(0, true);
  if (payload.byteLength < 2 + mimeLen) throw new Error('clipboard image mime truncated');
  const mime = new TextDecoder().decode(new Uint8Array(payload, 2, mimeLen));
  const bytes = new Uint8Array(payload, 2 + mimeLen);
  return { mime, bytes };
}

/// Decoded payload of a `MSG_VIDEO_FRAME` from the `/client` endpoint.
/// Layout (after the 8-byte proto header, big-endian unless noted):
///   bytes 0-3   : frame_id (u32 BE)            -- on the wire, not surfaced
///   bytes 4-11  : ping_echo_client_ts (f64 BE; 0.0 when flags & FLAG_HAS_PING == 0)
///   bytes 12-19 : capture_to_encode_ms (f64 BE) -- on the wire, not surfaced
///   bytes 20..  : raw Annex-B H.264 NAL data
///
/// frame_id and capture_to_encode_ms are still sent by the server but the
/// client doesn't read them today, so they're skipped rather than decoded.
export interface VideoFramePayload {
  isKeyframe: boolean;
  pingEchoClientTs: number | null;
  /** Annex-B H.264 data starting from offset 20 of the framed payload. */
  data: Uint8Array;
}

/**
 * Parses the video payload occupying `[byteOffset, byteOffset + byteLength)` of
 * `buf`. The defaults span the whole buffer, so callers passing an exact-length
 * payload still work; the live client passes the full WebSocket frame with
 * offset 8 so the returned `data` is a *view* over the received buffer (no
 * per-frame copy) -- the very buffer the decode worker then transfers.
 */
export function parseVideoFramePayload(
  buf: ArrayBuffer,
  flags: number,
  byteOffset = 0,
  byteLength = buf.byteLength - byteOffset,
): VideoFramePayload {
  if (byteLength < 20) {
    throw new Error(`MSG_VIDEO_FRAME payload too short: ${byteLength} (need >= 20)`);
  }
  const view = new DataView(buf, byteOffset, byteLength);
  const isKeyframe = (flags & FLAG_KEYFRAME) !== 0;
  const hasPing = (flags & FLAG_HAS_PING) !== 0;
  const pingRaw = view.getFloat64(4, false);
  return {
    isKeyframe,
    pingEchoClientTs: hasPing ? pingRaw : null,
    data: new Uint8Array(buf, byteOffset + 20, byteLength - 20),
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

export function parseAudioFramePayload(
  buf: ArrayBuffer,
  byteOffset = 0,
  byteLength = buf.byteLength - byteOffset,
): AudioFramePayload {
  if (byteLength < 8) {
    throw new Error(`MSG_AUDIO_FRAME payload too short: ${byteLength} (need >= 8)`);
  }
  const view = new DataView(buf, byteOffset, byteLength);
  // JS numbers can exactly represent integers up to 2^53; PTS values for
  // audio (microseconds from stream start) stay well within that range.
  const high = view.getUint32(0, false);
  const low = view.getUint32(4, false);
  return {
    ptsUs: high * 2 ** 32 + low,
    data: new Uint8Array(buf, byteOffset + 8, byteLength - 8),
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