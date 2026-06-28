import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import { persisted } from './persisted';

// This runner has no localStorage (Node's is gated behind a flag), so provide a
// minimal in-memory stand-in. It lets the test exercise persisted()'s real
// read/write paths rather than only its graceful no-storage fallback.
function fakeLocalStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => (map.has(k) ? map.get(k)! : null),
    setItem: (k: string, v: string) => void map.set(k, String(v)),
    removeItem: (k: string) => void map.delete(k),
    key: (i: number) => Array.from(map.keys())[i] ?? null,
  } as Storage;
}

describe('persisted', () => {
  beforeEach(() => {
    vi.stubGlobal('localStorage', fakeLocalStorage());
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('uses the initial value when storage is empty', () => {
    const store = persisted('k.empty', 42);
    expect(get(store)).toBe(42);
  });

  it('reads a previously stored value over the initial', () => {
    localStorage.setItem('k.read', JSON.stringify('saved'));
    const store = persisted('k.read', 'fallback');
    expect(get(store)).toBe('saved');
  });

  it('writes updates back to localStorage', () => {
    const store = persisted('k.write', { on: false });
    store.set({ on: true });
    expect(JSON.parse(localStorage.getItem('k.write')!)).toEqual({ on: true });
  });

  it('falls back to the initial value when the stored JSON is corrupt', () => {
    localStorage.setItem('k.corrupt', '{not valid json');
    const store = persisted('k.corrupt', 'default');
    expect(get(store)).toBe('default');
  });

  it('does not throw when localStorage access fails', () => {
    vi.spyOn(globalThis.localStorage, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(globalThis.localStorage, 'setItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    const store = persisted('k.blocked', 7);
    expect(get(store)).toBe(7);
    expect(() => store.set(8)).not.toThrow();
  });
});
