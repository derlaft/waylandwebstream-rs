import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AudioStream } from './audio';

// ─── Minimal Web Audio / WebCodecs stubs ─────────────────────────────────────

class FakeAudioContext {
  static instances: FakeAudioContext[] = [];

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

  createBufferSource = vi.fn(() => ({
    buffer: null as unknown,
    connect: vi.fn(),
    start: vi.fn(),
  }));

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
});