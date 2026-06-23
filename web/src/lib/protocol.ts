// Mirrors the wire protocol implemented by the Rust server. Keep this file
// in sync with the source of truth: src/server.rs (SignalingMessage),
// src/input/touch.rs (TouchEvent/TouchPoint), src/input/mouse.rs
// (MouseEvent/PointerPoint), src/input/keyboard.rs (KeyboardEvent), and the
// /stream binary frame format documented in src/server.rs next to
// `encode_video_frame`.

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

/// Messages the client sends over the `/ws` control channel.
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
    };

/// Messages the server pushes to the client over `/ws`.
export type ServerMessage =
  | { type: 'bitrate'; bps: number }
  | { type: 'codec'; codec: string };

/// `/stream` binary frame format, one WebSocket message per H.264 frame:
///   byte 0     : frame_type (0 = delta, 1 = key)
///   bytes 1-4  : frame_id (u32, big-endian)
///   byte 5     : has_ping_echo (0 or 1)
///   bytes 6-13 : ping_echo_client_ts (f64, big-endian; valid only if byte 5 == 1)
///   bytes 14.. : raw Annex-B H.264 for the whole frame
///
/// The ping echo round-trips a `ping` this client sent (see `VideoStream.sendPing`)
/// back on whichever frame next leaves the server's encoder -- comparing
/// `pingEchoClientTs` against this client's own clock on arrival gives a
/// round-trip latency measurement spanning network + the whole server
/// pipeline, without needing synchronized clocks.
export const STREAM_FRAME_HEADER_BYTES = 14;

export interface StreamFrameHeader {
  isKeyframe: boolean;
  frameId: number;
  pingEchoClientTs: number | null;
}

export function parseStreamFrameHeader(buf: ArrayBuffer): StreamFrameHeader {
  const view = new DataView(buf);
  const hasPingEcho = view.getUint8(5) === 1;
  return {
    isKeyframe: view.getUint8(0) === 1,
    frameId: view.getUint32(1, false),
    pingEchoClientTs: hasPingEcho ? view.getFloat64(6, false) : null,
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
