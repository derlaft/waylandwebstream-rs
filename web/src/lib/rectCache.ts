// A cached `getBoundingClientRect()` for an element whose box rarely changes
// but is read on high-frequency events (every pointermove/wheel, 60-120 Hz).
// Each read is a forced synchronous layout, so doing it per event is wasteful
// on the very canvas the decoder is also painting on the main thread.
//
// Reading live stays correct through resizes/rotation/scroll -- the reason the
// callers did it per event in the first place. A ResizeObserver gives back that
// exact correctness for free: it fires whenever the observed box actually
// changes size (including the debounced CSS resize the Viewport applies, which
// emits no window event), and a capture-phase scroll listener covers the box
// *moving* without resizing (page/visual-viewport scroll), which ResizeObserver
// doesn't report. Between those, the cached rect is always current.
//
// Where ResizeObserver isn't available (jsdom under tests, very old engines)
// this transparently falls back to a live read per call -- identical behavior
// to before, just unoptimized.

export interface RectCache {
  /// The element's current bounding rect, from cache when possible.
  get(): DOMRect;
  /// Detach observers/listeners. Safe to call once.
  dispose(): void;
}

export function observeRect(el: Element): RectCache {
  if (typeof ResizeObserver === 'undefined') {
    return { get: () => el.getBoundingClientRect(), dispose: () => {} };
  }

  let rect = el.getBoundingClientRect();
  const refresh = (): void => {
    rect = el.getBoundingClientRect();
  };

  const ro = new ResizeObserver(refresh);
  ro.observe(el);
  // Capture phase so a scroll anywhere up the ancestor chain (not just the
  // window) refreshes the cached position; passive since we never preventDefault.
  window.addEventListener('scroll', refresh, { capture: true, passive: true });
  window.visualViewport?.addEventListener('scroll', refresh);
  window.visualViewport?.addEventListener('resize', refresh);

  return {
    get: () => rect,
    dispose: () => {
      ro.disconnect();
      window.removeEventListener('scroll', refresh, true);
      window.visualViewport?.removeEventListener('scroll', refresh);
      window.visualViewport?.removeEventListener('resize', refresh);
    },
  };
}
