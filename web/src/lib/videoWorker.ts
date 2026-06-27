// Stage B: the H.264 decode + WebGL render pipeline, relocated off the main
// thread. Running VideoDecoder and the GL blit here means main-thread work
// (Svelte reactivity, input handlers, GC) can no longer stall the decoder's
// `output` callback -- the failure mode Stage A only narrowed by removing the
// readback. The whole VideoStream runs in here unchanged; only its two sinks
// are redirected: control messages and diagnostics are posted to the main
// thread (which owns the WebSocket and the stats store) instead of being
// called directly.
//
// All timing (arrival stamps, ping client_ts, decode latency) is taken with
// this worker's `performance.now()`, so it stays internally consistent even
// though the worker's clock origin differs from the main thread's.
import type { ClientMessage } from './protocol';
import { VideoStream, type StreamReporters } from './stream';
import type { MainToWorker, WorkerToMain } from './videoWorkerMessages';

// `self` is typed against the DOM lib here (the project doesn't pull in the
// WebWorker lib, which would conflict). Narrow it to just the worker surface
// we use rather than fight the lib mismatch.
const workerSelf = self as unknown as {
  onmessage: ((e: MessageEvent<MainToWorker>) => void) | null;
  postMessage(message: WorkerToMain, transfer?: Transferable[]): void;
};

function post(message: WorkerToMain): void {
  workerSelf.postMessage(message);
}

let stream: VideoStream | null = null;

workerSelf.onmessage = (e: MessageEvent<MainToWorker>) => {
  const data = e.data;
  switch (data.type) {
    case 'probe': {
      // Cheap throwaway context on a 2x2 canvas: tells the main thread whether
      // this worker can do WebGL before it commits the real canvas to us.
      let supported = false;
      try {
        const probe = new OffscreenCanvas(2, 2);
        supported = !!(probe.getContext('webgl') || probe.getContext('webgl2'));
      } catch {
        supported = false;
      }
      post({ type: 'webglProbe', supported });
      break;
    }
    case 'init': {
      const sendControl = (msg: ClientMessage) => post({ type: 'control', msg });
      const reporters: StreamReporters = {
        setResolution: (width, height) => post({ type: 'setResolution', width, height }),
        reportArrivalStats: (stats) => post({ type: 'arrivalStats', stats }),
        reportDecodeStats: (stats) => post({ type: 'decodeStats', stats }),
        reportEndToEndLatency: (ms) => post({ type: 'endToEnd', ms }),
      };
      stream = new VideoStream({ canvas: data.canvas, sendControl, reporters });
      // Started here rather than on a separate message: no frames arrive
      // until the main thread forwards them, so there's nothing to miss.
      stream.start();
      post({ type: 'rendererBackend', backend: stream.rendererBackend });
      break;
    }
    case 'frame':
      stream?.handleVideoFrame(data.frame);
      break;
    case 'codec':
      stream?.setCodec(data.codec);
      break;
    case 'close':
      stream?.close();
      stream = null;
      break;
  }
};
