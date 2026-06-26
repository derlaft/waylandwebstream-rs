import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AudioStream } from './audio';
import { FakeWebSocket, installFakeWebSocket } from './fakeWebSocket';
import { parseAudioPts, AUDIO_FRAME_HEADER_BYTES } from './protocol';

// ─── Minimal Web Audio / WebCodecs stubs ─────────────────────────────────────

class FakeAudioContext {
  static instances: FakeAudioContext[] = [];

  state: AudioContextState = 'suspended';
  currentTime = 0;
  readonly destination = {} as AudioDestinationNode;

  resume = vi.fn().mockImplementation(() => {
    this.state = 'running';
    for (const cb of this._stateListeners) cb();
    return Promise.resolve();
  });

  close = vi.fn().mockResolvedValue(undefined);

  private _stateListeners: Array<() => void> = [];

  addEventListener(type: string, cb: EventListenerOrEventListenerObject): void {
    if (type === 'statechange') this._stateListeners.push(cb as () => void);
  }
  removeEventListener(): void { /* unused in these tests */ }

  createBuffer = vi.fn((_ch: number, frames: number, rate: number) => ({
    duration: frames / rate,
    numberOfChannels: _ch,
    getChannelData: vi.fn(() => new Float32Array(frames)),
  }));

  createBufferSource = vi.fn(() => ({
    buffer: null as unknown,
    connect: vi.fn(),
    start: vi.fn(),
  }));

  constructor() {
    FakeAudioContext.instances.push(this);
  }
}

class FakeAudioDecoder {
  static instances: FakeAudioDecoder[] = [];

  state: 'unconfigured' | 'configured' | 'closed' = 'unconfigured';
  readonly decoded: EncodedAudioChunk[] = [];

  private _output: (d: AudioData) => void;

  configure = vi.fn(() => { this.state = 'configured'; });

  decode = vi.fn((chunk: EncodedAudioChunk) => {
    this.decoded.push(chunk);
    const fakeData = {
      numberOfChannels: 2,
      numberOfFrames: 960,
      sampleRate: 48_000,
      format: 'f32-planar' as AudioSampleFormat,
      copyTo: vi.fn(),
      close: vi.fn(),
    } as unknown as AudioData;
    this._output(fakeData);
  });

  close = vi.fn(() => { this.state = 'closed'; });

  constructor(init: { output: (d: AudioData) => void; error: (e: Error) => void }) {
    this._output = init.output;
    FakeAudioDecoder.instances.push(this);
  }
}

// Capture the last set of constructor args passed to EncodedAudioChunk so
// tests can assert on what was sent to the decoder.
let lastChunkInit: ConstructorParameters<typeof EncodedAudioChunk>[0] | null = null;

class FakeEncodedAudioChunk {
  timestamp: number;
  type: string;
  data: unknown;
  constructor(init: ConstructorParameters<typeof EncodedAudioChunk>[0]) {
    this.timestamp = init.timestamp;
    this.type = init.type;
    this.data = init.data;
    lastChunkInit = init;
  }
}

// ─── Test helpers ─────────────────────────────────────────────────────────────

/** Build an /audio wire frame: 8-byte big-endian pts_us + opus payload. */
function makeAudioFrame(pts_us: number, opusBytes: number[]): ArrayBuffer {
  const buf = new ArrayBuffer(AUDIO_FRAME_HEADER_BYTES + opusBytes.length);
  const view = new DataView(buf);
  const hi = Math.floor(pts_us / 2 ** 32);
  const lo = pts_us >>> 0;
  view.setUint32(0, hi, false);
  view.setUint32(4, lo, false);
  const arr = new Uint8Array(buf);
  for (let i = 0; i < opusBytes.length; i++) arr[AUDIO_FRAME_HEADER_BYTES + i] = opusBytes[i];
  return buf;
}

function installFakeAudioAPIs(): void {
  FakeAudioContext.instances = [];
  FakeAudioDecoder.instances = [];
  lastChunkInit = null;
  vi.stubGlobal('AudioContext', FakeAudioContext);
  vi.stubGlobal('AudioDecoder', FakeAudioDecoder);
  vi.stubGlobal('EncodedAudioChunk', FakeEncodedAudioChunk);
}

// ─── parseAudioPts ────────────────────────────────────────────────────────────

describe('parseAudioPts', () => {
  it('returns 0 for a zero PTS', () => {
    expect(parseAudioPts(new ArrayBuffer(8))).toBe(0);
  });

  it('parses a 20 ms PTS (20000 µs)', () => {
    const buf = new ArrayBuffer(8);
    new DataView(buf).setUint32(4, 20_000, false);
    expect(parseAudioPts(buf)).toBe(20_000);
  });

  it('parses a PTS spanning both u32 halves', () => {
    // 5 000 000 000 µs ≈ 5000 s — exercises the high word
    const pts = 5_000_000_000;
    const buf = new ArrayBuffer(8);
    const view = new DataView(buf);
    view.setUint32(0, Math.floor(pts / 2 ** 32), false);
    view.setUint32(4, pts >>> 0, false);
    expect(parseAudioPts(buf)).toBe(pts);
  });
});

// ─── AudioStream behaviour ────────────────────────────────────────────────────

describe('AudioStream', () => {
  beforeEach(() => {
    installFakeWebSocket();
    installFakeAudioAPIs();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  // Convenience: the AudioContext created by the most recent connect() call.
  function ctx(): FakeAudioContext { return FakeAudioContext.instances.at(-1)!; }

  describe('reconnect', () => {
    it('reconnects with backoff after an unexpected close', () => {
      vi.spyOn(Math, 'random').mockReturnValue(1);
      const setTimeoutSpy = vi.spyOn(globalThis, 'setTimeout');

      const stream = new AudioStream();
      stream.connect();
      expect(FakeWebSocket.instances).toHaveLength(1);

      FakeWebSocket.instances[0].simulateClose();
      expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 500);
      expect(FakeWebSocket.instances).toHaveLength(1);

      vi.runOnlyPendingTimers();
      expect(FakeWebSocket.instances).toHaveLength(2);

      FakeWebSocket.instances[1].simulateClose();
      expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 1000);

      // A successful open resets the counter so next close backs off from the start.
      vi.runOnlyPendingTimers();
      FakeWebSocket.instances[2].simulateOpen();
      FakeWebSocket.instances[2].simulateClose();
      expect(setTimeoutSpy).toHaveBeenLastCalledWith(expect.any(Function), 500);
    });

    it('does not reconnect after an intentional close()', () => {
      const stream = new AudioStream();
      stream.connect();
      stream.close();

      vi.runAllTimers();
      expect(FakeWebSocket.instances).toHaveLength(1);
    });

    it('does not reconnect when server reports audio capture unavailable', () => {
      const stream = new AudioStream();
      stream.connect();

      FakeWebSocket.instances[0].simulateClose({
        code: 1000,
        reason: 'audio capture not available',
      });

      vi.runAllTimers();
      // Still only one socket — no reconnect
      expect(FakeWebSocket.instances).toHaveLength(1);
    });
  });

  describe('message handling', () => {
    it('connects to /audio', () => {
      const stream = new AudioStream();
      stream.connect();
      expect(FakeWebSocket.instances[0].url).toContain('/audio');
    });

    it('sets binaryType to arraybuffer', () => {
      const stream = new AudioStream();
      stream.connect();
      expect(FakeWebSocket.instances[0].binaryType).toBe('arraybuffer');
    });

    it('decodes an incoming audio frame with the correct PTS and type', () => {
      const stream = new AudioStream();
      stream.connect();
      FakeWebSocket.instances[0].simulateOpen();

      const pts_us = 40_000;
      const opusBytes = [0xAA, 0xBB, 0xCC];
      const buf = makeAudioFrame(pts_us, opusBytes);

      FakeWebSocket.instances[0].onmessage?.({ data: buf });

      expect(lastChunkInit).not.toBeNull();
      expect(lastChunkInit!.timestamp).toBe(pts_us);
      expect(lastChunkInit!.type).toBe('key');
      const payload = new Uint8Array(lastChunkInit!.data as ArrayBuffer);
      expect(Array.from(payload)).toEqual(opusBytes);
    });
  });

  describe('AudioContext autoplay resume', () => {
    it('calls resume() on the first pointerdown gesture', () => {
      const stream = new AudioStream();
      stream.connect();
      const audioCtx = ctx();

      expect(audioCtx.resume).not.toHaveBeenCalled();
      document.dispatchEvent(new Event('pointerdown', { bubbles: true }));
      expect(audioCtx.resume).toHaveBeenCalledTimes(1);
      stream.close();
    });

    it('calls resume() on the first keydown gesture', () => {
      const stream = new AudioStream();
      stream.connect();
      const audioCtx = ctx();

      document.dispatchEvent(new Event('keydown', { bubbles: true }));
      expect(audioCtx.resume).toHaveBeenCalledTimes(1);
      stream.close();
    });

    it('removes gesture listeners after first successful resume', async () => {
      const stream = new AudioStream();
      stream.connect();
      const audioCtx = ctx();

      // First gesture: resumes and unregisters handlers via .then()
      document.dispatchEvent(new Event('pointerdown', { bubbles: true }));
      await Promise.resolve(); // let resume().then() run
      expect(audioCtx.resume).toHaveBeenCalledTimes(1);

      // Second gesture: handlers already removed, no second call
      document.dispatchEvent(new Event('pointerdown', { bubbles: true }));
      expect(audioCtx.resume).toHaveBeenCalledTimes(1);

      stream.close();
    });

    it('removes gesture listeners when close() is called', () => {
      const stream = new AudioStream();
      stream.connect();
      const audioCtx = ctx();
      stream.close();

      document.dispatchEvent(new Event('pointerdown', { bubbles: true }));
      expect(audioCtx.resume).not.toHaveBeenCalled();
    });
  });
});
