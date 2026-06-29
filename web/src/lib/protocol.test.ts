import { describe, expect, it } from 'vitest';
import {
  FLAG_HAS_PING,
  FLAG_KEYFRAME,
  HEADER_LEN,
  MSG_AUDIO_FRAME,
  MSG_CLIENT_MSG,
  MSG_CONTROL,
  MSG_VIDEO_FRAME,
  encodeClientMessage,
  encodeUnifiedFrame,
  parseAudioFramePayload,
  parseUnifiedHeader,
  parseVideoFramePayload,
} from './protocol';

describe('parseUnifiedHeader', () => {
  it('decodes msg_type, flags, and payload_len (little-endian u32) from the 8-byte header', () => {
    const buf = new ArrayBuffer(HEADER_LEN);
    const view = new DataView(buf);
    view.setUint8(0, MSG_VIDEO_FRAME);
    view.setUint8(1, FLAG_KEYFRAME);
    view.setUint16(2, 0, true); // reserved
    view.setUint32(4, 0x1234_5678, true);

    expect(parseUnifiedHeader(buf)).toEqual({
      msgType: MSG_VIDEO_FRAME,
      flags: FLAG_KEYFRAME,
      payloadLen: 0x1234_5678,
    });
  });

  it('throws when the buffer is shorter than 8 bytes', () => {
    const buf = new ArrayBuffer(3);
    expect(() => parseUnifiedHeader(buf)).toThrow();
  });
});

describe('encodeUnifiedFrame', () => {
  it('round-trips a 0-byte payload (header-only frame)', () => {
    const framed = encodeUnifiedFrame(MSG_CONTROL, 0, new Uint8Array(0));
    expect(framed.byteLength).toBe(HEADER_LEN);
    expect(parseUnifiedHeader(framed)).toEqual({
      msgType: MSG_CONTROL,
      flags: 0,
      payloadLen: 0,
    });
  });

  it('preserves the payload bytes verbatim after the header', () => {
    const payload = new Uint8Array([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42]);
    const framed = encodeUnifiedFrame(MSG_AUDIO_FRAME, FLAG_HAS_PING, payload);
    const header = parseUnifiedHeader(framed);
    expect(header.msgType).toBe(MSG_AUDIO_FRAME);
    expect(header.flags).toBe(FLAG_HAS_PING);
    expect(header.payloadLen).toBe(payload.byteLength);
    expect(Array.from(new Uint8Array(framed, HEADER_LEN))).toEqual(Array.from(payload));
  });

  it('accepts an ArrayBuffer payload too (same wire format)', () => {
    const payload = new ArrayBuffer(4);
    new Uint8Array(payload).set([1, 2, 3, 4]);
    const framed = encodeUnifiedFrame(MSG_CLIENT_MSG, 0, payload);
    expect(parseUnifiedHeader(framed).payloadLen).toBe(4);
    expect(Array.from(new Uint8Array(framed, HEADER_LEN))).toEqual([1, 2, 3, 4]);
  });
});

describe('encodeClientMessage', () => {
  it('produces a MSG_CLIENT_MSG frame whose payload is the JSON-encoded message', () => {
    const framed = encodeClientMessage({ type: 'ready' });
    const header = parseUnifiedHeader(framed);
    expect(header.msgType).toBe(MSG_CLIENT_MSG);
    expect(header.flags).toBe(0);
    const json = new TextDecoder().decode(new Uint8Array(framed, HEADER_LEN));
    expect(JSON.parse(json)).toEqual({ type: 'ready' });
  });

  it('handles messages with complex payloads (resize w/ both dims)', () => {
    const framed = encodeClientMessage({ type: 'resize', width: 1280, height: 720 });
    const header = parseUnifiedHeader(framed);
    expect(header.msgType).toBe(MSG_CLIENT_MSG);
    const json = new TextDecoder().decode(new Uint8Array(framed, HEADER_LEN, header.payloadLen));
    expect(JSON.parse(json)).toEqual({ type: 'resize', width: 1280, height: 720 });
  });
});

describe('parseVideoFramePayload', () => {
  it('throws on payloads shorter than the 20-byte prefix', () => {
    expect(() => parseVideoFramePayload(new ArrayBuffer(19), 0)).toThrow();
  });

  it('views H.264 data over the source buffer at an offset, without copying', () => {
    // Simulate a full WS frame: 8-byte unified header, 20-byte video prefix,
    // then the H.264 payload -- exactly how the live client calls it.
    const h264 = Uint8Array.of(1, 2, 3, 4, 5);
    const buf = new ArrayBuffer(8 + 20 + h264.length);
    new Uint8Array(buf).set(h264, 8 + 20);

    const parsed = parseVideoFramePayload(buf, 0, 8, 20 + h264.length);

    // No copy: the returned data is a view over the same ArrayBuffer, so the
    // buffer can be transferred to the worker zero-copy.
    expect(parsed.data.buffer).toBe(buf);
    expect(parsed.data.byteOffset).toBe(8 + 20);
    expect(Array.from(parsed.data)).toEqual([1, 2, 3, 4, 5]);
  });

  it('throws when the offset/length window is shorter than the 20-byte prefix', () => {
    expect(() => parseVideoFramePayload(new ArrayBuffer(100), 0, 90, 19)).toThrow();
  });
});

describe('parseAudioFramePayload', () => {
  it('parses a u64 BE pts_us spanning both u32 halves', () => {
    const pts = 5_000_000_000;
    const payload = new ArrayBuffer(8);
    const view = new DataView(payload);
    view.setUint32(0, Math.floor(pts / 2 ** 32), false);
    view.setUint32(4, pts >>> 0, false);
    const out = parseAudioFramePayload(payload);
    expect(out.ptsUs).toBe(pts);
  });

  it('throws on payloads shorter than 8 bytes', () => {
    expect(() => parseAudioFramePayload(new ArrayBuffer(4))).toThrow();
  });
});