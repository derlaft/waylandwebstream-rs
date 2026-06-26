// Connects to /audio, decodes Opus via WebCodecs AudioDecoder, and schedules
// decoded PCM for gapless playback via Web Audio API precision scheduling.
//
// Wire format: [u64 pts_us BE][raw Opus bytes]
// One WebSocket message = one 20 ms Opus frame.
//
// Reconnect behavior mirrors VideoStream: exponential back-off, full state
// reset on reconnect, no reconnect when close() is called intentionally.
//
// AudioContext lifecycle: deferred until the first user gesture (pointerdown
// or keydown).  Creating it then guarantees it starts in 'running' state
// with no autoplay warning.  Opus packets that arrive before the first
// gesture are silently dropped — a non-issue in practice because the user
// has to interact with the page to control the remote desktop.

import { nextBackoffDelayMs } from './backoff';
import { AUDIO_FRAME_HEADER_BYTES, parseAudioPts } from './protocol';

const SAMPLE_RATE = 48_000;
const CHANNELS = 2;
// Playback scheduling lookahead: schedule a decoded buffer this many seconds
// ahead of audioCtx.currentTime so we stay glitch-free even under GC pauses.
const SCHEDULE_AHEAD_S = 0.05;

export class AudioStream {
  private ws: WebSocket | null = null;
  private decoder: AudioDecoder | null = null;
  private audioCtx: AudioContext | null = null;
  // Wall-clock time at which the next decoded buffer should start playing.
  private nextPlayTime = 0;

  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedByCaller = false;

  // Capture-phase gesture listeners that create the AudioContext on first
  // interaction. Stored so they can be cleaned up on close().
  private gestureHandlers: Array<[string, EventListener]> = [];

  connect(): void {
    this.closedByCaller = false;
    this.connectSocket();
    this.installGestureCreate();
  }

  close(): void {
    this.closedByCaller = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close();
    this.ws = null;
    if (this.decoder && this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    this.decoder = null;
    this.audioCtx?.close();
    this.audioCtx = null;
    this.removeGestureHandlers();
  }

  // Called once on first user gesture: creates AudioContext (which starts in
  // 'running' state when created inside a gesture handler) and AudioDecoder.
  private installGestureCreate(): void {
    const handler = () => {
      if (this.audioCtx) return; // Already initialised by a prior gesture
      this.audioCtx = new AudioContext({ sampleRate: SAMPLE_RATE });
      // Defensive: if the context is later suspended (browser background
      // policy) and then resumed, reset nextPlayTime so we don't schedule
      // frames seconds in the past.
      this.audioCtx.addEventListener('statechange', () => {
        if (this.audioCtx?.state === 'running') {
          this.nextPlayTime = 0;
        }
      });
      this.setupDecoder();
      this.removeGestureHandlers();
    };
    const handlers: Array<[string, EventListener]> = [
      ['pointerdown', handler as EventListener],
      ['keydown', handler as EventListener],
    ];
    for (const [type, h] of handlers) {
      document.addEventListener(type, h, { capture: true });
    }
    this.gestureHandlers = handlers;
  }

  private removeGestureHandlers(): void {
    for (const [type, h] of this.gestureHandlers) {
      document.removeEventListener(type, h, { capture: true });
    }
    this.gestureHandlers = [];
  }

  private setupDecoder(): void {
    this.decoder = new AudioDecoder({
      output: (audioData) => this.scheduleAudio(audioData),
      error: (e) => {
        console.error('AudioDecoder error:', e);
        this.recoverDecoder();
      },
    });
    this.decoder.configure({
      codec: 'opus',
      sampleRate: SAMPLE_RATE,
      numberOfChannels: CHANNELS,
    });
  }

  private recoverDecoder(): void {
    if (this.decoder && this.decoder.state !== 'closed') {
      try { this.decoder.close(); } catch (_) { /* ignore */ }
    }
    this.setupDecoder();
  }

  private scheduleAudio(audioData: AudioData): void {
    const ctx = this.audioCtx;
    if (!ctx) { audioData.close(); return; }

    const buffer = ctx.createBuffer(
      audioData.numberOfChannels,
      audioData.numberOfFrames,
      audioData.sampleRate,
    );

    for (let ch = 0; ch < audioData.numberOfChannels; ch++) {
      const channelData = buffer.getChannelData(ch);
      // Specify f32-planar so the browser deinterleaves into the per-channel
      // Float32Array; without this, interleaved f32 AudioData would try to
      // copy all channels into a single-channel buffer and throw RangeError.
      audioData.copyTo(channelData, { planeIndex: ch, format: 'f32-planar' });
    }
    audioData.close();

    // Gapless precision scheduling: each buffer starts right after the previous
    // one.  If we've fallen behind (e.g. reconnect or context-resume after
    // suspension), snap to now + lookahead so we don't schedule in the past.
    const startAt = Math.max(ctx.currentTime + SCHEDULE_AHEAD_S, this.nextPlayTime);
    this.nextPlayTime = startAt + buffer.duration;

    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    source.start(startAt);
  }

  private connectSocket(): void {
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${wsProtocol}//${window.location.host}/audio`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      this.reconnectAttempt = 0;
      this.nextPlayTime = 0;
    };
    ws.onmessage = (event) => this.onAudioMessage(event.data as ArrayBuffer);
    ws.onerror = (e) => console.error('Audio stream error:', e);
    ws.onclose = (event) => {
      // Server sends this when PipeWire capture failed to start at launch.
      // Reconnecting won't help — audio capture is a one-time init.
      if (event.reason === 'audio capture not available') {
        console.info('Audio capture not available on server; audio disabled.');
        return;
      }
      this.scheduleReconnect();
    };
    this.ws = ws;
  }

  private scheduleReconnect(): void {
    if (this.closedByCaller) return;
    // Only recover the decoder if the AudioContext already exists (i.e. the
    // user has already interacted). Before the first gesture, decoder is null
    // and packets are dropped anyway, so there is nothing to recover.
    if (this.audioCtx) {
      this.recoverDecoder();
    }
    this.nextPlayTime = 0;
    const delay = nextBackoffDelayMs(this.reconnectAttempt);
    this.reconnectAttempt += 1;
    console.info(`Audio stream closed, reconnecting in ${Math.round(delay)}ms`);
    this.reconnectTimer = setTimeout(() => this.connectSocket(), delay);
  }

  private onAudioMessage(buf: ArrayBuffer): void {
    const decoder = this.decoder;
    if (!decoder || decoder.state === 'closed') return;

    const pts_us = parseAudioPts(buf);
    const data = new Uint8Array(buf, AUDIO_FRAME_HEADER_BYTES);

    const chunk = new EncodedAudioChunk({
      // Opus doesn't have I/P frame distinction; every packet is independently
      // decodable after the decoder is configured.
      type: 'key',
      timestamp: pts_us,
      data,
    });

    try {
      decoder.decode(chunk);
    } catch (e) {
      console.error('AudioDecoder.decode error:', e);
      this.recoverDecoder();
    }
  }
}
