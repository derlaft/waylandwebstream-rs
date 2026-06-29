// Computes the server-side render resolution from the viewport, and keeps
// the canvas's CSS size in sync, top-left aligned. By default render
// resolution is in CSS px, *not* multiplied by devicePixelRatio: capture/
// encode cost scales with pixel count, and on a DPR=2 display matching
// device pixels would mean encoding 4x the pixels for a sharpness gain
// that isn't always worth the CPU (a softer-than-native image on HiDPI is
// the default trade-off). The `nativeResolution` toggle opts back into
// device-pixel rendering for a crisp, native-resolution image.
import { writable } from 'svelte/store';
import type { ClientMessage } from './protocol';

/// When `true`, render resolution is multiplied by devicePixelRatio so the
/// stream is rendered at the display's native device-pixel resolution
/// instead of CSS px -- crisp on HiDPI screens at the cost of encoding
/// more pixels. Default `false`. Toggled by the native-resolution button.
export const nativeResolution = writable(false);

// Mirrors the CLI default in src/config.rs (`--max-resolution`, default
// "3840x2160"). The server is authoritative: it clamps every resize request
// to its configured max server-side (`sanitize_resolution` in src/config.rs,
// applied in src/main.rs's resize handler), since the request arrives over
// the untrusted /client socket. This client-side cap is just a conservative
// safety net so we don't render/send something absurd on very high-DPI or
// multi-monitor setups. The server doesn't advertise its actual configured
// value over the wire, so if it's set below this the client may still send a
// larger request and have it clamped -- harmless, just a wasted round-trip.
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

export function computeRenderResolution(pixelRatio = 1): RenderResolution {
  const { width, height } = getViewportCssSize();
  return {
    width: Math.min(floorToAlignment(width * pixelRatio), MAX_RENDER_WIDTH),
    height: Math.min(floorToAlignment(height * pixelRatio), MAX_RENDER_HEIGHT),
  };
}

export interface ViewportOptions {
  canvas: HTMLCanvasElement;
  sendControl: (msg: ClientMessage) => void;
}

export class Viewport {
  private readonly canvas: HTMLCanvasElement;
  private readonly sendControl: (msg: ClientMessage) => void;

  private useNative = false;
  private lastSent: RenderResolution | null = null;
  private debounceTimer: ReturnType<typeof setTimeout> | null = null;
  private unsubscribeNative: (() => void) | null = null;

  constructor(opts: ViewportOptions) {
    this.canvas = opts.canvas;
    this.sendControl = opts.sendControl;
  }

  start(): void {
    this.unsubscribeNative = nativeResolution.subscribe((native) => {
      this.useNative = native;
      this.update();
    });

    window.addEventListener('resize', this.handleWindowResize);
    window.addEventListener('orientationchange', this.handleWindowResize);
    // visualViewport events fire on discrete mobile changes (URL bar
    // show/hide, soft keyboard). Apply them immediately so the canvas CSS
    // size and touch-coordinate denominator are always current. The debounce
    // only guards against the continuous stream of events from desktop
    // drag-resize (window.resize), which these are not.
    window.visualViewport?.addEventListener('resize', this.handleVisualViewportChange);
    window.visualViewport?.addEventListener('scroll', this.handleVisualViewportChange);

    this.update();
  }

  stop(): void {
    this.unsubscribeNative?.();
    this.unsubscribeNative = null;

    window.removeEventListener('resize', this.handleWindowResize);
    window.removeEventListener('orientationchange', this.handleWindowResize);
    window.visualViewport?.removeEventListener('resize', this.handleVisualViewportChange);
    window.visualViewport?.removeEventListener('scroll', this.handleVisualViewportChange);

    if (this.debounceTimer !== null) {
      clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }
  }

  private handleWindowResize = (): void => {
    if (this.debounceTimer !== null) {
      clearTimeout(this.debounceTimer);
    }
    this.debounceTimer = setTimeout(() => this.update(), RESIZE_DEBOUNCE_MS);
  };

  private handleVisualViewportChange = (): void => {
    this.update();
  };

  private update(): void {
    // devicePixelRatio is read live: it changes with browser zoom and when
    // the window moves between monitors of different DPI, both of which also
    // fire window 'resize', so this path re-runs and re-sends as needed.
    const pixelRatio = this.useNative ? window.devicePixelRatio || 1 : 1;
    const render = computeRenderResolution(pixelRatio);
    const viewport = getViewportCssSize();

    // Canvas CSS size always fills the full viewport edge-to-edge, even
    // though the render resolution sent to the server is a few px smaller
    // (the /16 flooring) -- the browser stretches the canvas's bitmap to
    // whatever CSS box it's given for free, so there's no visual cost.
    // Sizing the CSS box to
    // `render` instead, as before, left a sub-16px dead strip at the
    // right/bottom edge of the viewport that was part of `.stage` but
    // outside the canvas -- and so outside every touch/pointer listener,
    // since those only ever attach to the canvas itself. Negligible on a
    // wide desktop viewport, but a much bigger fraction of a narrow phone
    // screen, where it read as touches near the edge being swallowed.
    // Top-left alignment within the black full-viewport container is
    // plain static CSS on Stage.svelte; this only ever sets width/height.
    this.canvas.style.width = `${viewport.width}px`;
    this.canvas.style.height = `${viewport.height}px`;

    if (this.lastSent && this.lastSent.width === render.width && this.lastSent.height === render.height) {
      return;
    }
    this.lastSent = render;
    this.sendControl({ type: 'resize', width: render.width, height: render.height });
  }
}
