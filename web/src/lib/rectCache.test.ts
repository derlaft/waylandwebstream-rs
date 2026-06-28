import { afterEach, describe, expect, it, vi } from 'vitest';
import { observeRect } from './rectCache';

type ROCallback = () => void;

// A minimal ResizeObserver stand-in: jsdom doesn't implement one, so the cached
// path of observeRect never runs without this. Captures the callback so a test
// can fire it, mimicking the box changing size.
class FakeResizeObserver {
  static last: FakeResizeObserver | null = null;
  cb: ROCallback;
  observed: Element[] = [];
  disconnected = false;
  constructor(cb: ROCallback) {
    this.cb = cb;
    FakeResizeObserver.last = this;
  }
  observe(el: Element): void {
    this.observed.push(el);
  }
  disconnect(): void {
    this.disconnected = true;
  }
  fire(): void {
    this.cb();
  }
}

function makeEl(rects: DOMRect[]): Element {
  const el = document.createElement('div');
  let i = 0;
  vi.spyOn(el, 'getBoundingClientRect').mockImplementation(
    () => rects[Math.min(i++, rects.length - 1)],
  );
  return el;
}

const rect = (left: number, width: number): DOMRect =>
  ({ left, top: 0, width, height: width, right: left + width, bottom: width, x: left, y: 0, toJSON: () => ({}) }) as DOMRect;

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  FakeResizeObserver.last = null;
});

describe('observeRect (no ResizeObserver -- jsdom/test fallback)', () => {
  it('reads the rect live on every get()', () => {
    // jsdom has no ResizeObserver, so this is the default path here.
    expect(typeof ResizeObserver).toBe('undefined');
    const el = makeEl([rect(0, 100), rect(5, 200)]);
    const cache = observeRect(el);
    expect(cache.get().width).toBe(100);
    expect(cache.get().width).toBe(200); // live: reflects the next read
    expect(() => cache.dispose()).not.toThrow();
  });
});

describe('observeRect (with ResizeObserver -- cached path)', () => {
  it('caches the rect and only refreshes when the observer fires', () => {
    vi.stubGlobal('ResizeObserver', FakeResizeObserver);
    const el = makeEl([rect(0, 100), rect(5, 200)]);
    const cache = observeRect(el);

    // First read is cached from construction; a second read does NOT re-measure.
    expect(cache.get().width).toBe(100);
    expect(cache.get().width).toBe(100);

    // The box changed: firing the observer refreshes the cache.
    FakeResizeObserver.last!.fire();
    expect(cache.get().width).toBe(200);

    cache.dispose();
    expect(FakeResizeObserver.last!.disconnected).toBe(true);
  });

  it('refreshes on scroll', () => {
    vi.stubGlobal('ResizeObserver', FakeResizeObserver);
    const el = makeEl([rect(0, 100), rect(50, 100)]);
    const cache = observeRect(el);
    expect(cache.get().left).toBe(0);
    window.dispatchEvent(new Event('scroll'));
    expect(cache.get().left).toBe(50);
    cache.dispose();
  });
});
