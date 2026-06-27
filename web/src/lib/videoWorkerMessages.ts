// Message contract between the main thread (lib/videoClient.ts) and the video
// decode/render worker (lib/videoWorker.ts). Types only -- no runtime -- so
// both sides can import it without dragging code across the worker boundary.
import type { ClientMessage, VideoFramePayload } from './protocol';
import type { ArrivalStats, DecodeStats } from './stream';

export type MainToWorker =
  // Ask the worker whether it can create a WebGL context at all (some Android
  // GPUs/drivers support WebGL on the main thread but not in a worker). Sent
  // before `init` -- if unsupported the main thread won't transfer the canvas
  // and uses the main-thread pipeline instead.
  | { type: 'probe' }
  // Hands the worker control of the on-screen canvas. The OffscreenCanvas is
  // transferred (not cloned); the worker owns all drawing to it afterward.
  | { type: 'init'; canvas: OffscreenCanvas }
  // One decoded VIDEO_FRAME payload. The H.264 bytes (`frame.data.buffer`)
  // are transferred, so the main thread must not touch them after sending.
  | { type: 'frame'; frame: VideoFramePayload }
  // Server reported a new WebCodecs codec string (e.g. an H.264 level change).
  | { type: 'codec'; codec: string }
  // Tear down: close the decoder and stop timers.
  | { type: 'close' };

export type WorkerToMain =
  // Reply to `probe`: whether a WebGL context is obtainable in this worker.
  | { type: 'webglProbe'; supported: boolean }
  // Reported once after `init`: which backend the worker's renderer ended up
  // using, for the debug panel.
  | { type: 'rendererBackend'; backend: 'webgl' | 'webgl2' | '2d' }
  // A control message the worker wants sent to the server (ping, latency
  // report, request_keyframe) -- the WebSocket lives on the main thread.
  | { type: 'control'; msg: ClientMessage }
  // Diagnostics destined for the stats store, which only the main thread can
  // touch. Mirror the StreamReporters surface one-to-one.
  | { type: 'setResolution'; width: number; height: number }
  | { type: 'arrivalStats'; stats: ArrivalStats }
  | { type: 'decodeStats'; stats: DecodeStats }
  | { type: 'endToEnd'; ms: number };
