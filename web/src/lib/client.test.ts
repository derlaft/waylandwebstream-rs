import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ClientChannel } from './client';
import { FakeWebSocket, installFakeWebSocket } from './fakeWebSocket';
import {
  FLAG_HAS_PING,
  FLAG_KEYFRAME,
  HEADER_LEN,
  MSG_AUDIO_FRAME,
  MSG_CLIENT_MSG,
  MSG_CONTROL,
  MSG_VIDEO_FRAME,
  encodeUnifiedFrame,
  parseUnifiedHeader,
} from './protocol';

// ─── Helpers ─────────────────────────────────────────────────────────────────

/** Reads one MSG_CLIENT_MSG frame from `sent` and returns its JSON body. */
function readClientMessageSent(sent: Array<string | ArrayBuffer | Uint8Array>): unknown {
  const last = sent[sent.length - 1];
  expect(last).toBeDefined();
  if (typeof last !== 'object' || !(last instanceof ArrayBuffer || last instanceof Uint8Array)) {
    throw new Error(`expected ArrayBuffer/Uint8Array, got ${typeof last}`);
  }
  const bytes: Uint8Array = last instanceof Uint8Array
    ? last
    : new Uint8Array(last);
  // Copy into a fresh ArrayBuffer so the type is unambiguously ArrayBuffer
  // (Uint8Array.buffer is typed as ArrayBuffer | SharedArrayBuffer).
  const copy = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(copy).set(bytes);
  const header = parseUnifiedHeader(copy);
  expect(header.msgType).toBe(MSG_CLIENT_MSG);
  const payload = new Uint8Array(copy, HEADER_LEN, header.payloadLen);
  return JSON.parse(new TextDecoder().decode(payload));
}

/** Build a server→client MSG_VIDEO_FRAME binary message. */
function makeVideoFrame(opts: {
  isKeyframe?: boolean;
  hasPing?: boolean;
  frameId?: number;
  pingEcho?: number;
  captureToEncodeMs?: number;
  h264?: Uint8Array;
}): ArrayBuffer {
  const flags = (opts.isKeyframe ? FLAG_KEYFRAME : 0) | (opts.hasPing ? FLAG_HAS_PING : 0);
  const h264 = opts.h264 ?? new Uint8Array([0xAA, 0xBB, 0xCC]);
  const payload = new Uint8Array(20 + h264.byteLength);
  const view = new DataView(payload.buffer);
  view.setUint32(0, opts.frameId ?? 42, false);
  view.setFloat64(4, opts.hasPing ? (opts.pingEcho ?? 1234.5) : 0, false);
  view.setFloat64(12, opts.captureToEncodeMs ?? 5.5, false);
  payload.set(h264, 20);
  return encodeUnifiedFrame(MSG_VIDEO_FRAME, flags, payload);
}

/** Build a server→client MSG_AUDIO_FRAME binary message. */
function makeAudioFrame(ptsUs: number, opus: Uint8Array): ArrayBuffer {
  const payload = new Uint8Array(8 + opus.byteLength);
  const view = new DataView(payload.buffer);
  view.setUint32(0, Math.floor(ptsUs / 2 ** 32), false);
  view.setUint32(4, ptsUs >>> 0, false);
  payload.set(opus, 8);
  return encodeUnifiedFrame(MSG_AUDIO_FRAME, 0, payload);
}

/** Build a server→client MSG_CONTROL binary message with a JSON ServerMessage. */
function makeControlMessage(msg: unknown): ArrayBuffer {
  const json = new TextEncoder().encode(JSON.stringify(msg));
  return encodeUnifiedFrame(MSG_CONTROL, 0, json);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe('ClientChannel', () => {
  beforeEach(() => {
    installFakeWebSocket();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  describe('connection', () => {
    it('connects to /client', () => {
      const channel = new ClientChannel();
      channel.connect();
      expect(FakeWebSocket.instances[0].url).toContain('/client');
      channel.close();
    });

    it('sets binaryType to arraybuffer so onmessage delivers ArrayBuffer', () => {
      const channel = new ClientChannel();
      channel.connect();
      expect(FakeWebSocket.instances[0].binaryType).toBe('arraybuffer');
      channel.close();
    });
  });

  describe('send', () => {
    it('sends a framed MSG_CLIENT_MSG (8-byte header + JSON body) on open', () => {
      const channel = new ClientChannel();
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();

      channel.send({ type: 'ready' });
      const body = readClientMessageSent(FakeWebSocket.instances[0].sent);
      expect(body).toEqual({ type: 'ready' });
      channel.close();
    });

    it('sends a request_keyframe message with the right framing', () => {
      const channel = new ClientChannel();
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();

      channel.send({ type: 'request_keyframe' });
      const body = readClientMessageSent(FakeWebSocket.instances[0].sent);
      expect(body).toEqual({ type: 'request_keyframe' });
      channel.close();
    });

    it('queues sends while the socket is not yet OPEN and flushes them on open', () => {
      const channel = new ClientChannel();
      channel.connect();
      // Don't simulateOpen yet.
      channel.send({ type: 'request_keyframe' });
      expect(FakeWebSocket.instances[0].sent).toHaveLength(0);

      FakeWebSocket.instances[0].simulateOpen();
      const body = readClientMessageSent(FakeWebSocket.instances[0].sent);
      expect(body).toEqual({ type: 'request_keyframe' });
      channel.close();
    });
  });

  describe('reconnect', () => {
    it('does not auto-reconnect after an unexpected close', () => {
      const setTimeoutSpy = vi.spyOn(globalThis, 'setTimeout');
      const channel = new ClientChannel();
      channel.connect();
      expect(FakeWebSocket.instances).toHaveLength(1);

      FakeWebSocket.instances[0].simulateClose();
      // No timer scheduled and no new socket: the connection stays closed
      // until reconnect() is called explicitly.
      expect(setTimeoutSpy).not.toHaveBeenCalled();
      vi.runAllTimers();
      expect(FakeWebSocket.instances).toHaveLength(1);
      channel.close();
    });

    it('fires onClosed on an unexpected close but not on intentional close()', () => {
      const onClosed = vi.fn();
      const channel = new ClientChannel({ onClosed });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();

      FakeWebSocket.instances[0].simulateClose();
      expect(onClosed).toHaveBeenCalledTimes(1);

      // reconnect() opens a fresh socket; an intentional close() must not
      // fire onClosed.
      channel.reconnect();
      expect(FakeWebSocket.instances).toHaveLength(2);
      channel.close();
      expect(onClosed).toHaveBeenCalledTimes(1);
    });

    it('reconnect() re-opens after an unexpected close and flushes queued sends', () => {
      const channel = new ClientChannel();
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateClose();

      // Sends while closed are dropped (stale input from the disconnect).
      channel.send({ type: 'request_keyframe' });

      channel.reconnect();
      const reconnected = FakeWebSocket.instances[1];
      expect(reconnected).toBeDefined();
      // A send queued after reconnect() (while CONNECTING) flushes on open.
      channel.send({ type: 'request_keyframe' });
      expect(reconnected.sent).toHaveLength(0);
      reconnected.simulateOpen();
      // `ready` (sent on every open) plus the queued request_keyframe.
      expect(reconnected.sent).toHaveLength(2);
      expect(readClientMessageSent([reconnected.sent[0]])).toEqual({ type: 'ready' });
      expect(readClientMessageSent([reconnected.sent[1]])).toEqual({ type: 'request_keyframe' });
      channel.close();
    });

    it('reconnect() is a no-op while open or after an intentional close()', () => {
      const channel = new ClientChannel();
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      // Open: no new socket.
      channel.reconnect();
      expect(FakeWebSocket.instances).toHaveLength(1);

      // Intentional close: reconnect() must not revive it.
      channel.close();
      channel.reconnect();
      expect(FakeWebSocket.instances).toHaveLength(1);
    });
  });

  describe('inbound dispatch', () => {
    it('invokes onCodec when MSG_CONTROL codec is received', () => {
      const onCodec = vi.fn();
      const channel = new ClientChannel({ onCodec });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeControlMessage({ type: 'codec', codec: 'avc1.42E028' }),
      );
      expect(onCodec).toHaveBeenCalledWith('avc1.42E028');
      channel.close();
    });

    it('invokes onCursor when MSG_CONTROL cursor is received', () => {
      const onCursor = vi.fn();
      const channel = new ClientChannel({ onCursor });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeControlMessage({ type: 'cursor', cursor: { kind: 'hidden' } }),
      );
      expect(onCursor).toHaveBeenCalledWith({ kind: 'hidden' });
      channel.close();
    });

    it('invokes onVideoFrame with parsed VIDEO_FRAME payload', () => {
      const onVideoFrame = vi.fn();
      const channel = new ClientChannel({ onVideoFrame });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeVideoFrame({
          isKeyframe: true,
          hasPing: true,
          frameId: 7,
          pingEcho: 100.5,
          captureToEncodeMs: 8.25,
          h264: new Uint8Array([0xDE, 0xAD, 0xBE, 0xEF]),
        }),
      );
      expect(onVideoFrame).toHaveBeenCalledTimes(1);
      const frame = onVideoFrame.mock.calls[0][0];
      expect(frame.isKeyframe).toBe(true);
      expect(frame.frameId).toBe(7);
      expect(frame.pingEchoClientTs).toBeCloseTo(100.5);
      expect(frame.captureToEncodeMs).toBeCloseTo(8.25);
      expect(Array.from(frame.data)).toEqual([0xDE, 0xAD, 0xBE, 0xEF]);
      channel.close();
    });

    it('reports pingEchoClientTs as null when FLAG_HAS_PING is clear', () => {
      const onVideoFrame = vi.fn();
      const channel = new ClientChannel({ onVideoFrame });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeVideoFrame({ isKeyframe: false, frameId: 8, h264: new Uint8Array([0x01]) }),
      );
      expect(onVideoFrame.mock.calls[0][0].pingEchoClientTs).toBeNull();
      channel.close();
    });

    it('invokes onAudioFrame with parsed AUDIO_FRAME payload', () => {
      const onAudioFrame = vi.fn();
      const channel = new ClientChannel({ onAudioFrame });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeAudioFrame(40_000, new Uint8Array([0xAA, 0xBB])),
      );
      expect(onAudioFrame).toHaveBeenCalledTimes(1);
      const f = onAudioFrame.mock.calls[0][0];
      expect(f.ptsUs).toBe(40_000);
      expect(Array.from(f.data)).toEqual([0xAA, 0xBB]);
      channel.close();
    });

    it('drops a malformed (too-short) frame instead of closing the connection', () => {
      const onVideoFrame = vi.fn();
      const onAudioFrame = vi.fn();
      const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
      const channel = new ClientChannel({ onVideoFrame, onAudioFrame });
      channel.connect();
      FakeWebSocket.instances[0].simulateOpen();

      // 3 bytes is below HEADER_LEN; should be ignored, not crash.
      FakeWebSocket.instances[0].simulateBinaryMessage(new ArrayBuffer(3));

      expect(onVideoFrame).not.toHaveBeenCalled();
      expect(onAudioFrame).not.toHaveBeenCalled();
      expect(errorSpy).toHaveBeenCalled();
      // Still operational: a valid frame after the bad one is delivered.
      FakeWebSocket.instances[0].simulateBinaryMessage(
        makeVideoFrame({ frameId: 9, h264: new Uint8Array([0x42]) }),
      );
      expect(onVideoFrame).toHaveBeenCalledTimes(1);
      channel.close();
    });
  });
});