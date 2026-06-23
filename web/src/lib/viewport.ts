// Computes the server-side render resolution from the viewport, and keeps
// the canvas's CSS size in sync, top-left aligned. Render resolution is in
// CSS px, *not* multiplied by devicePixelRatio: capture/encode cost scales
// with pixel count, and on a DPR=2 display matching device pixels would
// mean encoding 4x the pixels for a sharpness gain that isn't worth the
// CPU (a softer-than-native image on HiDPI is the accepted trade-off).
import { writable } from 'svelte/store';
import type { ClientMessage } from './protocol';

/// Default `1`; a future `2` halves render resolution further for
/// low-power links (the deferred 2x client-side scale button). Exposed as
/// a store so that button can flip it later without any other module
/// changing.
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
  const { width, height } = getViewportCssSize();
  return {
    width: Math.min(floorToAlignment(width / scale), MAX_RENDER_WIDTH),
    height: Math.min(floorToAlignment(height / scale), MAX_RENDER_HEIGHT),
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
    const render = computeRenderResolution(this.currentScale);

    // Canvas CSS size = render * scaleFactor -- approximately the full
    // viewport minus the sub-16px flooring remainder. Top-left alignment
    // within the black full-viewport container is plain static CSS on
    // Stage.svelte; this only ever sets width/height.
    this.canvas.style.width = `${render.width * this.currentScale}px`;
    this.canvas.style.height = `${render.height * this.currentScale}px`;

    if (this.lastSent && this.lastSent.width === render.width && this.lastSent.height === render.height) {
      return;
    }
    this.lastSent = render;
    this.sendControl({ type: 'resize', width: render.width, height: render.height });
  }
}
