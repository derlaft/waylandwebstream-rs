// Opus AudioDecoder + Web Audio playback scheduling. The transport (the
// `/client` unified WebSocket) lives in lib/client.ts; this module consumes
// `AudioFramePayload` objects that client.ts hands it, decodes them via
// WebCodecs, and schedules the PCM onto the AudioContext. Mirror of
// stream.ts for the audio half.
//
// AudioContext lifecycle: deferred until the first user gesture (pointerdown
// or keydown). Creating it then guarantees it starts in 'running' state
// with no autoplay warning. Opus packets that arrive before the first
// gesture are silently dropped -- a non-issue in practice because the user
// has to interact with the page to control the remote desktop.
//
// Reconnect behavior: managed by ClientChannel. After a reconnect we
// receive fresh `handleAudioFrame` calls and may receive any in-flight
// audio packets the client happened to grab during the outage; since we
// always create a fresh decoder on `start()`, the old decoder's pending
// output callbacks are discarded cleanly.

import type { AudioFramePayload } from './protocol';

const SAMPLE_RATE = 48_000;
const CHANNELS = 2;
// Playback scheduling lookahead: schedule a decoded buffer this many seconds
// ahead of audioCtx.currentTime so we stay glitch-free even under GC pauses.
const SCHEDULE_AHEAD_S = 0.05;
// Maximum the playback head (`nextPlayTime`) may run ahead of the live
// AudioContext clock before we force a resync. Audio is chained gaplessly via
// `nextPlayTime`, so a backlog burst (network/GC stall, or tab throttling that
// doesn't actually suspend the context) or slow sample-clock drift would
// otherwise push the lead up by 20 ms per buffer with nothing to pull it back
// -- audio ends up seconds late until the page is refreshed. When the lead
// exceeds this we drop the queued-ahead audio and snap back to the live edge.
// Kept comfortably above SCHEDULE_AHEAD_S, low enough to feel in sync.
const MAX_LEAD_S = 0.3;

export class AudioStream {
  private decoder: AudioDecoder | null = null;
  private audioCtx: AudioContext | null = null;
  // Wall-clock time at which the next decoded buffer should start playing.
  private nextPlayTime = 0;
  // Buffers we've scheduled but that haven't finished playing yet. Tracked so a
  // resync can stop() the ones still queued ahead of the live edge -- moving
  // nextPlayTime alone can't un-schedule sources that source.start() already
  // committed. Pruned via each source's onended.
  private pendingSources = new Set<AudioBufferSourceNode>();

  // Capture-phase gesture listeners that create the AudioContext on first
  // interaction. Stored so they can be cleaned up on close().
  private gestureHandlers: Array<[string, EventListener]> = [];

  constructor() {
    this.installGestureCreate();
  }

  /// Called by ClientChannel after the socket has connected and started
  /// delivering frames. No-op until the first user gesture (see
  /// `installGestureCreate`).
  start(): void {
    // Fresh decoder / state for a new session, in case a previous close()
    // left something behind. The AudioContext is intentionally created
    // later (on first gesture), so nothing to do here.
    this.nextPlayTime = 0;
  }

  close(): void {
    if (this.decoder && this.decoder.state !== 'closed') {
      try { this.decoder.close(); } catch (_) { /* ignore */ }
    }
    this.decoder = null;
    this.pendingSources.clear();
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
    // `capture: true` must match the registration flags exactly; otherwise
    // the DOM silently keeps the listener around and a stale one from a
    // previous test will fire alongside the new one, creating two
    // AudioContexts (this was a real failure mode caught in the audio tests).
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
    // Stale timing from before the fault would otherwise schedule the fresh
    // decoder's output relative to an old playback head.
    this.nextPlayTime = 0;
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

    // If the playback head has drifted too far ahead of the live clock -- a
    // backlog burst chained 20 ms at a time, or accumulated sample-clock drift
    // -- discard the queued-ahead audio and snap back to the live edge.
    // Otherwise the lead never shrinks and audio plays seconds late until the
    // page is refreshed.
    if (this.nextPlayTime - ctx.currentTime > MAX_LEAD_S) {
      for (const queued of this.pendingSources) {
        try { queued.stop(); } catch (_) { /* already started/ended */ }
      }
      this.pendingSources.clear();
      this.nextPlayTime = 0;
    }

    // Gapless precision scheduling: each buffer starts right after the previous
    // one. If we've fallen behind (e.g. reconnect or context-resume after
    // suspension) or just resynced above, snap to now + lookahead so we don't
    // schedule in the past.
    const startAt = Math.max(ctx.currentTime + SCHEDULE_AHEAD_S, this.nextPlayTime);
    this.nextPlayTime = startAt + buffer.duration;

    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    this.pendingSources.add(source);
    source.onended = () => { this.pendingSources.delete(source); };
    source.start(startAt);
  }

  /// Called by lib/client.ts for every MSG_AUDIO_FRAME it dispatches.
  /// Silently drops packets that arrive before the AudioContext exists (no
  /// user gesture yet) -- those would be unplayable anyway, and the
  /// WebCodecs decoder is intentionally not created until then either.
  handleAudioFrame(payload: AudioFramePayload): void {
    const decoder = this.decoder;
    if (!decoder || decoder.state === 'closed') return;

    const chunk = new EncodedAudioChunk({
      // Opus doesn't have I/P frame distinction; every packet is independently
      // decodable after the decoder is configured.
      type: 'key',
      timestamp: payload.ptsUs,
      data: payload.data,
    });

    try {
      decoder.decode(chunk);
    } catch (e) {
      console.error('AudioDecoder.decode error:', e);
      this.recoverDecoder();
    }
  }
}