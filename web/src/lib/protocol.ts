// Mirrors the wire protocol implemented by the Rust server. Keep this file
// in sync with the source of truth: src/server.rs (SignalingMessage),
// src/input/touch.rs (TouchEvent/TouchPoint), src/input/mouse.rs
// (MouseEvent/PointerPoint), and the /stream binary frame format documented
// in src/server.rs next to `encode_video_frame`.

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

/// Messages the client sends over the `/ws` control channel.
export type ClientMessage =
  | { type: 'ready' }
  | { type: 'resize'; width: number; height: number }
  | ({ type: 'touch' } & TouchMessage)
  | ({ type: 'pointer' } & PointerMessage)
  | { type: 'request_keyframe' }
  | {
      type: 'latency';
      encoding_ms?: number;
      network_ms?: number;
      jitter_buffer_ms?: number;
      decoding_ms?: number;
      total_ms: number;
    };

/// `/stream` binary frame format, one WebSocket message per H.264 frame:
///   byte 0    : frame_type (0 = delta, 1 = key)
///   bytes 1-4 : frame_id (u32, big-endian)
///   bytes 5.. : raw Annex-B H.264 for the whole frame
export const STREAM_FRAME_HEADER_BYTES = 5;

export interface StreamFrameHeader {
  isKeyframe: boolean;
  frameId: number;
}

export function parseStreamFrameHeader(buf: ArrayBuffer): StreamFrameHeader {
  const view = new DataView(buf);
  return {
    isKeyframe: view.getUint8(0) === 1,
    frameId: view.getUint32(1, false),
  };
}

/// Baseline 3.1, Annex-B, SPS/PPS repeated inline on keyframes -- matches
/// the x264 `profile=baseline level=3.1` settings in src/encoder/mod.rs, so
/// no out-of-band `description` is needed.
export const DECODER_CONFIG: VideoDecoderConfig = {
  codec: 'avc1.42E01F',
  optimizeForLatency: true,
};
