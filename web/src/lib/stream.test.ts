import { describe, expect, it } from 'vitest';
import { ensureCanvasSize } from './stream';

describe('ensureCanvasSize', () => {
  it('sizes the canvas to the first frame', () => {
    const canvas = document.createElement('canvas');
    expect(ensureCanvasSize(canvas, 1280, 720)).toBe(true);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);
  });

  it('is a no-op for further frames at the same resolution', () => {
    const canvas = document.createElement('canvas');
    ensureCanvasSize(canvas, 1280, 720);
    expect(ensureCanvasSize(canvas, 1280, 720)).toBe(false);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);
  });

  // Regression test for the bug where, on first page load, the /stream and
  // /ws sockets connect independently: the very first decoded frame can
  // arrive at the server's old/default resolution before a just-sent
  // resize request takes effect server-side. A one-shot "have we sized the
  // canvas yet" flag (the old implementation) would latch onto that first,
  // stale size forever, stretching every later, correctly-sized frame into
  // it -- producing wrong scaling/aspect ratio that only a full reload
  // (which gives the server a head start to already be at the right
  // resolution) would clear up.
  it('resizes again when a later frame arrives at a different resolution', () => {
    const canvas = document.createElement('canvas');

    // Old/default resolution frame, decoded before the resize took effect.
    ensureCanvasSize(canvas, 1280, 720);
    expect(canvas.width).toBe(1280);
    expect(canvas.height).toBe(720);

    // The resize has now taken effect server-side; a later frame arrives
    // at the actually-requested resolution and must resize the canvas.
    expect(ensureCanvasSize(canvas, 800, 592)).toBe(true);
    expect(canvas.width).toBe(800);
    expect(canvas.height).toBe(592);
  });

  it('resizes when only one dimension changes', () => {
    const canvas = document.createElement('canvas');
    ensureCanvasSize(canvas, 800, 592);
    expect(ensureCanvasSize(canvas, 800, 600)).toBe(true);
    expect(canvas.height).toBe(600);
  });
});
