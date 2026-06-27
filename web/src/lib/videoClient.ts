// Front-end for the video pipeline: decides between the Stage B worker path
// (decode + render off the main thread, via OffscreenCanvas) and the Stage A
// main-thread fallback, and presents one interface to Stage.svelte either
// way. The decode logic itself lives in lib/stream.ts (VideoStream); this
// module is only the wiring -- transport-side callbacks in, stats/control out.
import type { ClientMessage, VideoFramePayload } from './protocol';
import {
  reportArrivalStats,
  reportDecodeStats,
  reportEndToEndLatency,
  setResolution,
  setVideoPipelineInfo,
} from './stats';
import { VideoStream, type StreamReporters } from './stream';
import type { MainToWorker, WorkerToMain } from './videoWorkerMessages';

export interface VideoPipelineOptions {
  canvas: HTMLCanvasElement;
  /// Forwards a control message (ping, latency, request_keyframe) to the
  /// server. Owned by lib/client.ts's ClientChannel.
  sendControl: (msg: ClientMessage) => void;
}

export interface VideoPipeline {
  /// Begin decoding. For the worker path the decoder is already live (started
  /// on `init`); this only matters for the main-thread path.
  start(): void;
  /// Feed one decoded VIDEO_FRAME payload from the transport.
  handleVideoFrame(frame: VideoFramePayload): void;
  /// Apply a server codec change.
  setCodec(codec: string): void;
  /// Tear everything down (decoder, timers, worker).
  close(): void;
  /// Which path is live -- surfaced for diagnostics.
  readonly mode: 'worker' | 'main';
}

// The main-thread stats store can only be touched here, so the worker posts
// raw numbers back and we apply them. Same shape the worker would have called
// directly on the main-thread path.
const mainThreadReporters: StreamReporters = {
  setResolution,
  reportArrivalStats,
  reportDecodeStats,
  reportEndToEndLatency,
};

/// Stage A fallback: VideoStream on the main thread, exactly as before Stage B.
class MainThreadPipeline implements VideoPipeline {
  readonly mode = 'main';
  private readonly stream: VideoStream;

  /// `workerWebgl` is the probe outcome that led here: false = a worker was
  /// tried but couldn't do WebGL, null = no worker was attempted at all.
  constructor(opts: VideoPipelineOptions, workerWebgl: boolean | null) {
    this.stream = new VideoStream({
      canvas: opts.canvas,
      sendControl: opts.sendControl,
      reporters: mainThreadReporters,
    });
    setVideoPipelineInfo({
      pipelineMode: 'main',
      rendererBackend: this.stream.rendererBackend,
      workerWebgl,
    });
  }

  start(): void {
    this.stream.start();
  }

  handleVideoFrame(frame: VideoFramePayload): void {
    this.stream.handleVideoFrame(frame);
  }

  setCodec(codec: string): void {
    this.stream.setCodec(codec);
  }

  close(): void {
    this.stream.close();
  }
}

/// Stage B: decode + render in a worker against a transferred OffscreenCanvas.
class WorkerPipeline implements VideoPipeline {
  readonly mode = 'worker';
  private readonly worker: Worker;

  // The worker is created and WebGL-probed by `createVideoPipeline`, then
  // handed over here only once we know it can render. Transferring the canvas
  // is the point of no return -- after it, the canvas can't render on the main
  // thread -- so it only happens here, past the probe.
  constructor(opts: VideoPipelineOptions, worker: Worker) {
    this.worker = worker;
    this.worker.onmessage = (e: MessageEvent<WorkerToMain>) =>
      this.onWorkerMessage(e.data, opts.sendControl);
    this.worker.onerror = (e) => console.error('Video worker error:', e);

    const offscreen = opts.canvas.transferControlToOffscreen();
    this.post({ type: 'init', canvas: offscreen }, [offscreen]);
  }

  private post(message: MainToWorker, transfer?: Transferable[]): void {
    this.worker.postMessage(message, transfer ?? []);
  }

  start(): void {
    // No-op: the worker started its VideoStream on `init`.
  }

  handleVideoFrame(frame: VideoFramePayload): void {
    // Transfer the H.264 bytes (zero-copy). `frame` is owned by ClientChannel
    // for this one call and dropped right after, so detaching its buffer is
    // safe.
    this.post({ type: 'frame', frame }, [frame.data.buffer]);
  }

  setCodec(codec: string): void {
    this.post({ type: 'codec', codec });
  }

  close(): void {
    this.post({ type: 'close' });
    this.worker.terminate();
  }

  private onWorkerMessage(msg: WorkerToMain, sendControl: (msg: ClientMessage) => void): void {
    switch (msg.type) {
      case 'webglProbe':
        // Handled by the probe in createVideoPipeline before this handler is
        // installed; ignore any stray duplicate.
        return;
      case 'rendererBackend':
        // A WorkerPipeline only exists when the probe passed, so worker WebGL
        // is true here (unless the worker hit its own fallback to 2D).
        setVideoPipelineInfo({
          pipelineMode: 'worker',
          rendererBackend: msg.backend,
          workerWebgl: true,
        });
        return;
      case 'control':
        sendControl(msg.msg);
        return;
      case 'setResolution':
        setResolution(msg.width, msg.height);
        return;
      case 'arrivalStats':
        reportArrivalStats(msg.stats);
        return;
      case 'decodeStats':
        reportDecodeStats(msg.stats);
        return;
      case 'endToEnd':
        reportEndToEndLatency(msg.ms);
        return;
    }
  }
}

function workerPathSupported(canvas: HTMLCanvasElement): boolean {
  return (
    typeof Worker !== 'undefined' &&
    typeof OffscreenCanvas !== 'undefined' &&
    typeof canvas.transferControlToOffscreen === 'function' &&
    // WebCodecs presence on the main thread is a reliable proxy for its
    // presence in a worker (both Chromium and WebKit expose it in both, or
    // neither). The worker needs it; we can't probe the worker from here.
    typeof VideoDecoder !== 'undefined'
  );
}

// Ask a freshly-created worker whether it can do WebGL. Resolves false on a
// timeout so a worker that never answers (or fails to load) can't wedge
// startup -- the caller just uses the main-thread pipeline instead.
function probeWorkerWebGl(worker: Worker): Promise<boolean> {
  return new Promise((resolve) => {
    const finish = (supported: boolean) => {
      clearTimeout(timer);
      worker.removeEventListener('message', onMessage);
      resolve(supported);
    };
    const onMessage = (e: MessageEvent<WorkerToMain>) => {
      if (e.data.type === 'webglProbe') finish(e.data.supported);
    };
    const timer = setTimeout(() => finish(false), 2000);
    worker.addEventListener('message', onMessage);
    worker.postMessage({ type: 'probe' } satisfies MainToWorker);
  });
}

export async function createVideoPipeline(opts: VideoPipelineOptions): Promise<VideoPipeline> {
  if (workerPathSupported(opts.canvas)) {
    let worker: Worker | null = null;
    try {
      worker = new Worker(new URL('./videoWorker.ts', import.meta.url), { type: 'module' });
      // Only commit the canvas to the worker if it can actually render. Some
      // Android devices do WebGL on the main thread but not in a worker; there
      // the worker's 2D fallback would do a per-frame readback (wasteful on a
      // phone), so the main-thread WebGL pipeline is the better choice.
      if (await probeWorkerWebGl(worker)) {
        return new WorkerPipeline(opts, worker);
      }
      // Worker exists but can't do WebGL -> fall back, recording workerWebgl=false.
      worker.terminate();
      return new MainThreadPipeline(opts, false);
    } catch (e) {
      console.warn('Worker video pipeline init failed; using main thread:', e);
      worker?.terminate();
      return new MainThreadPipeline(opts, false);
    }
  }
  // No worker/OffscreenCanvas support at all: workerWebgl is not applicable.
  return new MainThreadPipeline(opts, null);
}
