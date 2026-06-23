// Computes the server-side render resolution from the viewport and DPR,
// and keeps the canvas's CSS size in sync -- 1:1 device pixels, top-left
// aligned, never CSS-upscaled/blurred. This is the fix for the old bug:
// the previous client resized in CSS px with no DPR awareness, so the
// canvas's CSS size ended up equal to its buffer size and got
// flex-centered, creating edge dead zones and a blurry sub-DPR image.
import { writable } from 'svelte/store';
import type { ClientMessage } from './protocol';

/// Default `1`; a future `2` halves render resolution for hidpi perf (the
/// deferred 2x client-side scale button). Exposed as a store so that
/// button can flip it later without any other module changing.
export const scaleFactor = writable(1);

// Mirrors the CLI default in src/config.rs (`--max-resolution`, default
// "3840x2160"). The server doesn't expose its actually-configured value
// over the wire and doesn't even clamp resize requests to it server-side
// today (see src/main.rs's resize handling), so this is only a
// conservative client-side sanity clamp against requesting something
// absurd on very high-DPI/multi-monitor setups -- not authoritative.
const MAX_RENDER_WIDTH = 3840;
const MAX_RENDER_HEIGHT = 2160;

const ALIGNMENT = 16;
const RESIZE_DEBOUNCE_MS = 300;

export interface RenderResolution {
  width: number;
  height: number;
}

function floorToAlignment(px: number): number {
  return Math.floor(px / ALIGNMENT) * ALIGNMENT;
}

function getViewportCssSize(): { width: number; height: number } {
  // visualViewport is correct under mobile browser chrome / a soft
  // keyboard, where innerWidth/Height don't shrink to match.
  const vv = window.visualViewport;
  if (vv) {
    return { width: vv.width, height: vv.height };
  }
  return { width: window.innerWidth, height: window.innerHeight };
}

export function computeRenderResolution(scale: number): RenderResolution {
  const dpr = window.devicePixelRatio || 1;
  const { width, height } = getViewportCssSize();
  return {
    width: Math.min(floorToAlignment((width * dpr) / scale), MAX_RENDER_WIDTH),
    height: Math.min(floorToAlignment((height * dpr) / scale), MAX_RENDER_HEIGHT),
  };
}

export interface ViewportOptions {
  canvas: HTMLCanvasElement;
  sendControl: (msg: ClientMessage) => void;
}

export class Viewport {
  private readonly canvas: HTMLCanvasElement;
  private readonly sendControl: (msg: ClientMessage) => void;

  private currentScale = 1;
  private lastSent: RenderResolution | null = null;
  private debounceTimer: ReturnType<typeof setTimeout> | null = null;
  private unsubscribeScale: (() => void) | null = null;

  constructor(opts: ViewportOptions) {
    this.canvas = opts.canvas;
    this.sendControl = opts.sendControl;
  }

  start(): void {
    this.unsubscribeScale = scaleFactor.subscribe((scale) => {
      this.currentScale = scale;
      this.update();
    });

    window.addEventListener('resize', this.handleViewportChange);
    window.addEventListener('orientationchange', this.handleViewportChange);
    window.visualViewport?.addEventListener('resize', this.handleViewportChange);
    window.visualViewport?.addEventListener('scroll', this.handleViewportChange);

    this.update();
  }

  stop(): void {
    this.unsubscribeScale?.();
    this.unsubscribeScale = null;

    window.removeEventListener('resize', this.handleViewportChange);
    window.removeEventListener('orientationchange', this.handleViewportChange);
    window.visualViewport?.removeEventListener('resize', this.handleViewportChange);
    window.visualViewport?.removeEventListener('scroll', this.handleViewportChange);

    if (this.debounceTimer !== null) {
      clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }
  }

  private handleViewportChange = (): void => {
    if (this.debounceTimer !== null) {
      clearTimeout(this.debounceTimer);
    }
    this.debounceTimer = setTimeout(() => this.update(), RESIZE_DEBOUNCE_MS);
  };

  private update(): void {
    const dpr = window.devicePixelRatio || 1;
    const render = computeRenderResolution(this.currentScale);

    // Canvas CSS size = render * scaleFactor / dpr -- approximately the
    // full viewport minus the sub-16px flooring remainder. Top-left
    // alignment within the black full-viewport container is plain static
    // CSS on Stage.svelte; this only ever sets width/height.
    this.canvas.style.width = `${(render.width * this.currentScale) / dpr}px`;
    this.canvas.style.height = `${(render.height * this.currentScale) / dpr}px`;

    if (this.lastSent && this.lastSent.width === render.width && this.lastSent.height === render.height) {
      return;
    }
    this.lastSent = render;
    this.sendControl({ type: 'resize', width: render.width, height: render.height });
  }
}
