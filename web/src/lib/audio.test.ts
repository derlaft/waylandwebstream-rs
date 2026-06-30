import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AudioStream } from './audio';

// ─── Minimal Web Audio / WebCodecs stubs ─────────────────────────────────────

interface FakeSource {
  buffer: unknown;
  onended: (() => void) | null;
  connect: ReturnType<typeof vi.fn>;
  start: ReturnType<typeof vi.fn>;
  stop: ReturnType<typeof vi.fn>;
}

class FakeAudioContext {
  static instances: FakeAudioContext[] = [];
  // Every source handed out by createBufferSource, across all instances, in
  // creation order -- lets tests inspect scheduled start times and stop() calls.
  static sources: FakeSource[] = [];

  state: AudioContextState = 'running'; // created inside gesture → immediately running
  currentTime = 0;
  readonly destination = {} as AudioDestinationNode;

  close = vi.fn().mockResolvedValue(undefined);

  private _stateListeners: Array<() => void> = [];

  addEventListener(type: string, cb: EventListenerOrEventListenerObject): void {
    if (type === 'statechange') this._stateListeners.push(cb as () => void);
  }
  removeEventListener(): void { /* unused */ }

  createBuffer = vi.fn((_ch: number, frames: number, rate: number) => ({
    duration: frames / rate,
    numberOfChannels: _ch,
    getChannelData: vi.fn(() => new Float32Array(frames)),
  }));

  createBufferSource = vi.fn((): FakeSource => {
    const source: FakeSource = {
      buffer: null,
      onended: null,
      connect: vi.fn(),
      start: vi.fn(),
      stop: vi.fn(),
    };
    FakeAudioContext.sources.push(source);
    return source;
  });

  constructor() { FakeAudioContext.instances.push(this); }
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

let lastChunkInit: ConstructorParameters<typeof EncodedAudioChunk>[0] | null = null;
class FakeEncodedAudioChunk {
  timestamp: number; type: string; data: unknown;
  constructor(init: ConstructorParameters<typeof EncodedAudioChunk>[0]) {
    this.timestamp = init.timestamp; this.type = init.type; this.data = init.data;
    lastChunkInit = init;
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function installFakeAudioAPIs(): void {
  FakeAudioContext.instances = [];
  FakeAudioContext.sources = [];
  FakeAudioDecoder.instances = [];
  lastChunkInit = null;
  vi.stubGlobal('AudioContext', FakeAudioContext);
  vi.stubGlobal('AudioDecoder', FakeAudioDecoder);
  vi.stubGlobal('EncodedAudioChunk', FakeEncodedAudioChunk);
}

/** Simulate a user gesture so AudioStream creates its AudioContext + Decoder. */
function simulateGesture(): void {
  document.dispatchEvent(new Event('pointerdown', { bubbles: true }));
}

// ─── AudioStream ─────────────────────────────────────────────────────────────

describe('AudioStream', () => {
  beforeEach(() => {
    installFakeAudioAPIs();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  describe('AudioContext is deferred until first gesture', () => {
    it('does not create AudioContext on construction', () => {
      const stream = new AudioStream();
      expect(FakeAudioContext.instances).toHaveLength(0);
      stream.close();
    });

    it('creates AudioContext on first pointerdown', () => {
      const stream = new AudioStream();
      simulateGesture();
      expect(FakeAudioContext.instances).toHaveLength(1);
      stream.close();
    });

    it('creates AudioContext on first keydown', () => {
      const stream = new AudioStream();
      document.dispatchEvent(new Event('keydown', { bubbles: true }));
      expect(FakeAudioContext.instances).toHaveLength(1);
      stream.close();
    });

    it('only creates one AudioContext across multiple gestures', () => {
      const stream = new AudioStream();
      simulateGesture();
      simulateGesture();
      expect(FakeAudioContext.instances).toHaveLength(1);
      stream.close();
    });

    it('removes gesture listeners after close() before first gesture', () => {
      const stream = new AudioStream();
      stream.close();
      simulateGesture();
      expect(FakeAudioContext.instances).toHaveLength(0);
    });

    it('drops audio frames received before the first gesture', () => {
      const stream = new AudioStream();
      stream.handleAudioFrame({ ptsUs: 0, data: new Uint8Array([0xAA]) });
      expect(lastChunkInit).toBeNull();
      stream.close();
    });
  });

  describe('message handling', () => {
    it('decodes an incoming audio frame with the correct PTS and type', () => {
      const stream = new AudioStream();
      simulateGesture(); // creates decoder

      const opusBytes = new Uint8Array([0xAA, 0xBB, 0xCC]);
      stream.handleAudioFrame({ ptsUs: 40_000, data: opusBytes });

      expect(lastChunkInit).not.toBeNull();
      expect(lastChunkInit!.timestamp).toBe(40_000);
      expect(lastChunkInit!.type).toBe('key');
      expect(Array.from(new Uint8Array(lastChunkInit!.data as ArrayBuffer))).toEqual([0xAA, 0xBB, 0xCC]);
      stream.close();
    });

    it('recovers the decoder after a decode error', () => {
      const stream = new AudioStream();
      simulateGesture();
      expect(FakeAudioDecoder.instances).toHaveLength(1);

      const firstDecoder = FakeAudioDecoder.instances[0];
      // Make the first decoder throw on the next decode; the stream should
      // catch, close the broken decoder, and stand up a fresh one.
      firstDecoder.decode = vi.fn(() => { throw new Error('boom'); });
      stream.handleAudioFrame({ ptsUs: 20_000, data: new Uint8Array([0xBB]) });

      expect(FakeAudioDecoder.instances).toHaveLength(2);
      expect(firstDecoder.close).toHaveBeenCalled();
      stream.close();
    });

    it('is a no-op when called after close()', () => {
      const stream = new AudioStream();
      simulateGesture();
      stream.close();
      // Should not throw, and shouldn't decode anything into a new decoder.
      stream.handleAudioFrame({ ptsUs: 0, data: new Uint8Array([0xCC]) });
      expect(FakeAudioDecoder.instances).toHaveLength(1);
    });
  });

  describe('playback scheduling / desync recovery', () => {
    // The FakeAudioDecoder emits one 960-sample (20 ms) buffer synchronously per
    // decode(), and FakeAudioContext.currentTime is whatever the test sets it to
    // -- so feeding frames while currentTime stays frozen simulates a backlog
    // burst decoded all at once, the situation that grows the lead unbounded.
    const startOf = (s: FakeSource) => s.start.mock.calls[0][0] as number;

    it('resyncs to the live edge when a burst pushes the lead past the cap', () => {
      const stream = new AudioStream();
      simulateGesture();
      const ctx = FakeAudioContext.instances[0];
      ctx.currentTime = 0; // live clock frozen: a pile of packets arriving at once

      for (let i = 0; i < 20; i++) {
        stream.handleAudioFrame({ ptsUs: i * 20_000, data: new Uint8Array([i]) });
      }

      // The resync stops the sources still queued ahead of the live edge...
      const stopped = FakeAudioContext.sources.filter((s) => s.stop.mock.calls.length > 0);
      expect(stopped.length).toBeGreaterThan(0);

      // ...and nothing is ever scheduled more than the cap (+lookahead) ahead of
      // the frozen clock, so latency stays bounded instead of climbing 20 ms per
      // buffer (without the fix this would reach ~0.45 s after 20 frames).
      const maxStart = Math.max(...FakeAudioContext.sources.map(startOf));
      expect(maxStart).toBeLessThanOrEqual(0.3 + 0.05 + 1e-9);

      stream.close();
    });

    it('does not resync during steady real-time playback', () => {
      const stream = new AudioStream();
      simulateGesture();
      const ctx = FakeAudioContext.instances[0];

      // Clock advances in lockstep with the 20 ms buffers: the lead stays at the
      // scheduling lookahead and never approaches the cap.
      for (let i = 0; i < 50; i++) {
        ctx.currentTime = i * 0.02;
        stream.handleAudioFrame({ ptsUs: i * 20_000, data: new Uint8Array([i & 0xff]) });
      }

      const anyStopped = FakeAudioContext.sources.some((s) => s.stop.mock.calls.length > 0);
      expect(anyStopped).toBe(false);

      stream.close();
    });

    it('drops a finished source from the pending set via onended', () => {
      const stream = new AudioStream();
      simulateGesture();

      stream.handleAudioFrame({ ptsUs: 0, data: new Uint8Array([0x01]) });
      const [source] = FakeAudioContext.sources;
      // The scheduler wires onended to prune the pending set; firing it must not
      // throw and leaves the set ready for the next buffer.
      expect(source.onended).toBeTypeOf('function');
      expect(() => source.onended!()).not.toThrow();

      stream.close();
    });
  });
});