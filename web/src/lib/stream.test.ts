import { describe, expect, it } from 'vitest';
import { ensureCanvasSize } from './stream';
import {
  FLAG_HAS_PING,
  FLAG_KEYFRAME,
  HEADER_LEN,
  MSG_VIDEO_FRAME,
  encodeUnifiedFrame,
  parseVideoFramePayload,
} from './protocol';

describe('ensureCanvasSize', () => {
  it('sizes the canvas to the first frame', () => {
    const canvas = document.createElement('canvas');
    expect(ensureCanvasSize(canvas, 1280, 720)).toBe(true);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);
  });

  it('is a no-op for further frames at the same resolution', () => {
    const canvas = document.createElement('canvas');
    ensureCanvasSize(canvas, 1280, 720);
    expect(ensureCanvasSize(canvas, 1280, 720)).toBe(false);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);
  });

  // Regression test for the bug where, on first page load, the socket and
  // viewport connect independently: the very first decoded frame can arrive
  // at the server's old/default resolution before a just-sent resize
  // request takes effect server-side. A one-shot "have we sized the canvas
  // yet" flag (the old implementation) would latch onto that first, stale
  // size forever, stretching every later, correctly-sized frame into it --
  // producing wrong scaling/aspect ratio that only a full reload (which
  // gives the server a head start to already be at the right resolution)
  // would clear up.
  it('resizes again when a later frame arrives at a different resolution', () => {
    const canvas = document.createElement('canvas');

    // Old/default resolution frame, decoded before the resize took effect.
    ensureCanvasSize(canvas, 1280, 720);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);

    // The resize has now taken effect server-side; a later frame arrives
    // at the actually-requested resolution and must resize the canvas.
    expect(ensureCanvasSize(canvas, 800, 592)).toBe(true);
    expect(canvas.width).toBe(800);
    expect(canvas.height).toBe(592);
  });

  it('resizes when only one dimension changes', () => {
    const canvas = document.createElement('canvas');
    ensureCanvasSize(canvas, 800, 592);
    expect(ensureCanvasSize(canvas, 800, 600)).toBe(true);
    expect(canvas.height).toBe(600);
  });
});

// ─── VIDEO_FRAME parsing ─────────────────────────────────────────────────────
//
// These tests pin the wire format the browser and Rust server must agree on
// byte-for-byte (see src/server.rs's `encode_unified_video_frame` and
// src/proto.rs). Any change to either side's framing here is a protocol
// break; the tests below catch it before a release.

function buildVideoPayload(opts: {
  isKeyframe: boolean;
  hasPing: boolean;
  frameId: number;
  pingEcho: number;
  captureToEncodeMs: number;
  h264: Uint8Array;
}): ArrayBuffer {
  const flags = (opts.isKeyframe ? FLAG_KEYFRAME : 0) | (opts.hasPing ? FLAG_HAS_PING : 0);
  const payload = new Uint8Array(20 + opts.h264.byteLength);
  const view = new DataView(payload.buffer);
  view.setUint32(0, opts.frameId, false);
  view.setFloat64(4, opts.pingEcho, false);
  view.setFloat64(12, opts.captureToEncodeMs, false);
  payload.set(opts.h264, 20);
  return encodeUnifiedFrame(MSG_VIDEO_FRAME, flags, payload);
}

/** Read the proto header flags byte (byte 1) from a framed message. */
function flagsByte(buf: ArrayBuffer): number {
  return new Uint8Array(buf)[1];
}

describe('parseVideoFramePayload', () => {
  it('parses a keyframe with ping echo and H.264 payload', () => {
    const buf = buildVideoPayload({
      isKeyframe: true,
      hasPing: true,
      frameId: 0x0102_0304,
      pingEcho: 12345.6789,
      captureToEncodeMs: 8.5,
      h264: new Uint8Array([0x67, 0x42, 0x00, 0x1F, 0xAA, 0xBB, 0xCC]),
    });
    // Skip past the proto header (HEADER_LEN bytes) to hand the parser
    // exactly the per-message payload.
    const payload = buf.slice(HEADER_LEN);
    const flags = flagsByte(buf);
    const parsed = parseVideoFramePayload(payload, flags);

    expect(parsed.isKeyframe).toBe(true);
    expect(parsed.pingEchoClientTs).toBeCloseTo(12345.6789);
    // frame_id (offset 0) and capture_to_encode_ms (offset 12) are still on the
    // wire (written by buildVideoPayload above) but the client skips them, so the
    // data starting at offset 20 is what proves the parser used the right offset.
    expect(Array.from(parsed.data)).toEqual([0x67, 0x42, 0x00, 0x1F, 0xAA, 0xBB, 0xCC]);
  });

  it('reports pingEchoClientTs as null when FLAG_HAS_PING is clear (the server writes 0.0)', () => {
    const buf = buildVideoPayload({
      isKeyframe: false,
      hasPing: false,
      frameId: 7,
      pingEcho: 0.0,
      captureToEncodeMs: 0.0,
      h264: new Uint8Array([0x41, 0x9A]),
    });
    const payload = buf.slice(HEADER_LEN);
    const flags = flagsByte(buf);
    const parsed = parseVideoFramePayload(payload, flags);

    expect(parsed.isKeyframe).toBe(false);
    expect(parsed.pingEchoClientTs).toBeNull();
    expect(Array.from(parsed.data)).toEqual([0x41, 0x9A]);
  });

  it('round-trips an empty H.264 payload (header-only chunks)', () => {
    // A defensive minimum: the encoder should never emit this in practice,
    // but the parser must not crash on it.
    const buf = buildVideoPayload({
      isKeyframe: true,
      hasPing: false,
      frameId: 1,
      pingEcho: 0.0,
      captureToEncodeMs: 0.0,
      h264: new Uint8Array(0),
    });
    const payload = buf.slice(HEADER_LEN);
    const flags = flagsByte(buf);
    const parsed = parseVideoFramePayload(payload, flags);

    expect(parsed.data.byteLength).toBe(0);
  });

  it('throws when the payload is too short to contain the 20-byte prefix', () => {
    const payload = new ArrayBuffer(10);
    expect(() => parseVideoFramePayload(payload, 0)).toThrow();
  });
});