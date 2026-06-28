import { writable, type Writable } from 'svelte/store';

/// A `writable` that mirrors itself into localStorage so a preference (the
/// on-screen-keyboard toggle, the floating button's position, the clipboard-
/// sync toggle) survives a reload. Falls back to `initial` when storage is
/// unavailable or holds garbage, and a failed persist never breaks the caller.
export function persisted<T>(key: string, initial: T): Writable<T> {
  let start = initial;
  try {
    const raw = localStorage.getItem(key);
    if (raw !== null) start = JSON.parse(raw) as T;
  } catch {
    /* storage blocked or value corrupt -- use the default */
  }
  const store = writable<T>(start);
  store.subscribe((value) => {
    try {
      localStorage.setItem(key, JSON.stringify(value));
    } catch {
      /* ignore: a failed persist shouldn't break anything */
    }
  });
  return store;
}
