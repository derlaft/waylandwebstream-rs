import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { attachInput } from './input';
import type { ClientMessage } from './protocol';

// ─── Touch stub ──────────────────────────────────────────────────────────────
// jsdom does not expose the Touch constructor globally. Define a minimal
// stand-in that gives normalizeTouches the properties it reads
// (clientX, clientY, identifier, force) and satisfies the TouchEvent init dict.
if (typeof globalThis.Touch === 'undefined') {
  class TouchStub {
    identifier: number;
    target: EventTarget;
    clientX: number;
    clientY: number;
    force: number;
    constructor(init: { identifier: number; target: EventTarget; clientX?: number; clientY?: number; force?: number }) {
      this.identifier = init.identifier;
      this.target = init.target;
      this.clientX = init.clientX ?? 0;
      this.clientY = init.clientY ?? 0;
      this.force = init.force ?? 0;
    }
  }
  (globalThis as unknown as Record<string, unknown>).Touch = TouchStub;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeTouch(id: number, clientX: number, clientY: number, target: EventTarget): Touch {
  return new Touch({ identifier: id, target, clientX, clientY, force: 0.5 }) as Touch;
}

function makeCanvas(width = 1920, height = 1080): HTMLCanvasElement {
  const canvas = document.createElement('canvas');
  // Stub getBoundingClientRect so coordinate normalization has stable numbers.
  vi.spyOn(canvas, 'getBoundingClientRect').mockReturnValue({
    left: 0,
    top: 0,
    width,
    height,
    right: width,
    bottom: height,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  } as DOMRect);
  // JSDOM doesn't implement pointer capture; stub it to avoid errors.
  canvas.setPointerCapture = vi.fn();
  canvas.releasePointerCapture = vi.fn();
  return canvas;
}

function collectMessages(canvas: HTMLCanvasElement): { messages: ClientMessage[]; detach: () => void } {
  const messages: ClientMessage[] = [];
  const { detach } = attachInput(canvas, (msg) => messages.push(msg));
  return { messages, detach };
}

// ─── Wheel delta normalization ────────────────────────────────────────────────

describe('wheel event delta normalization', () => {
  let canvas: HTMLCanvasElement;
  let messages: ClientMessage[];
  let detach: () => void;

  beforeEach(() => {
    canvas = makeCanvas(1920, 1080);
    ({ messages, detach } = collectMessages(canvas));
  });

  afterEach(() => {
    detach();
    vi.restoreAllMocks();
  });

  it('passes DOM_DELTA_PIXEL deltas through unchanged', () => {
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 50,
        deltaY: 100,
        deltaMode: WheelEvent.DOM_DELTA_PIXEL,
      }),
    );

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;
    expect(msg.type).toBe('pointer');
    expect(msg.eventType).toBe('wheel');
    expect(msg.deltaX).toBe(50);
    expect(msg.deltaY).toBe(100);
  });

  it('scales DOM_DELTA_LINE deltas to pixels', () => {
    // Firefox sends deltaMode=LINE with deltaY≈3 per scroll click.
    // After normalization those 3 units should be multiplied up to ~120px
    // so the server receives roughly the same magnitude Chrome sends in PIXEL mode (~100px).
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 0,
        deltaY: 3,
        deltaMode: WheelEvent.DOM_DELTA_LINE,
      }),
    );

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;
    // Should be significantly larger than the raw line-mode value.
    expect(msg.deltaY).toBeGreaterThan(3);
    // Should be in the same ballpark as Chrome's pixel-mode output (~100px).
    expect(msg.deltaY).toBeGreaterThanOrEqual(60);
    expect(msg.deltaY).toBeLessThanOrEqual(300);
    expect(msg.deltaX).toBe(0);
  });

  it('scales DOM_DELTA_LINE negative deltas (scroll up)', () => {
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 0,
        deltaY: -3,
        deltaMode: WheelEvent.DOM_DELTA_LINE,
      }),
    );

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;
    expect(msg.deltaY).toBeLessThan(-3);
    expect(msg.deltaX).toBe(0);
  });

  it('scales DOM_DELTA_PAGE deltas by canvas height', () => {
    // One page should map to the full canvas height (1080px).
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 0,
        deltaY: 1,
        deltaMode: WheelEvent.DOM_DELTA_PAGE,
      }),
    );

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;
    expect(msg.deltaY).toBe(1080);
    expect(msg.deltaX).toBe(0);
  });

  it('normalizes both axes for DOM_DELTA_LINE', () => {
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 2,
        deltaY: 3,
        deltaMode: WheelEvent.DOM_DELTA_LINE,
      }),
    );

    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;
    // Both axes must be scaled by the same factor (not just Y).
    expect(msg.deltaX).toBeGreaterThan(2);
    expect(msg.deltaY).toBeGreaterThan(3);
    expect(msg.deltaX / msg.deltaY).toBeCloseTo(2 / 3, 5);
  });

  it('produces identical magnitudes for equivalent PIXEL and LINE events', () => {
    // Chrome PIXEL scroll: deltaY = 120 (3 lines × Chrome's ~40px/line internally)
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 0,
        deltaY: 120,
        deltaMode: WheelEvent.DOM_DELTA_PIXEL,
      }),
    );
    const pixelMsg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;

    // Firefox LINE scroll: same physical event, deltaY = 3 lines
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        bubbles: true,
        clientX: 960,
        clientY: 540,
        deltaX: 0,
        deltaY: 3,
        deltaMode: WheelEvent.DOM_DELTA_LINE,
      }),
    );
    const lineMsg = messages[1] as Extract<ClientMessage, { type: 'pointer'; eventType: 'wheel' }>;

    // Both should produce the same output after normalization (40px/line × 3 = 120px).
    expect(lineMsg.deltaY).toBe(pixelMsg.deltaY);
  });
});

// ─── Keyboard events ──────────────────────────────────────────────────────────

describe('keyboard events', () => {
  let canvas: HTMLCanvasElement;
  let messages: ClientMessage[];
  let detach: () => void;

  beforeEach(() => {
    canvas = makeCanvas();
    ({ messages, detach } = collectMessages(canvas));
  });

  afterEach(() => {
    detach();
    vi.restoreAllMocks();
  });

  it('sends keydown with the physical key code', () => {
    canvas.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, code: 'KeyA', key: 'a' }));

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'key' }>;
    expect(msg.type).toBe('key');
    expect(msg.eventType).toBe('keydown');
    expect(msg.code).toBe('KeyA');
  });

  it('sends keyup', () => {
    canvas.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true, code: 'KeyA' }));

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'key' }>;
    expect(msg.type).toBe('key');
    expect(msg.eventType).toBe('keyup');
    expect(msg.code).toBe('KeyA');
  });

  it('drops repeated keydown events', () => {
    canvas.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, code: 'KeyA', repeat: true }));
    expect(messages).toHaveLength(0);
  });

  it('releases all held keys on canvas blur', () => {
    canvas.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, code: 'ShiftLeft' }));
    canvas.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, code: 'KeyA' }));
    expect(messages).toHaveLength(2);

    canvas.dispatchEvent(new FocusEvent('blur'));
    // Should have received keyup for both held keys.
    const keyups = messages.slice(2).filter((m) => m.type === 'key' && m.eventType === 'keyup');
    expect(keyups.map((m) => (m as Extract<ClientMessage, { type: 'key' }>).code).sort()).toEqual(
      ['KeyA', 'ShiftLeft'].sort(),
    );
  });
});

// ─── Pointer coordinate normalization ────────────────────────────────────────

describe('pointer coordinate normalization', () => {
  let canvas: HTMLCanvasElement;
  let messages: ClientMessage[];
  let detach: () => void;

  beforeEach(() => {
    canvas = makeCanvas(1920, 1080);
    ({ messages, detach } = collectMessages(canvas));
  });

  afterEach(() => {
    detach();
    vi.restoreAllMocks();
  });

  it('normalizes pointer position to [0, 1] relative to canvas bounds', () => {
    canvas.dispatchEvent(
      new PointerEvent('pointermove', {
        bubbles: true,
        clientX: 960,
        clientY: 270,
        pointerType: 'mouse',
      }),
    );

    expect(messages).toHaveLength(1);
    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'pointermove' }>;
    expect(msg.pointer.x).toBeCloseTo(0.5, 5);
    expect(msg.pointer.y).toBeCloseTo(0.25, 5);
  });

  it('clamps pointer positions outside the canvas to [0, 1]', () => {
    canvas.dispatchEvent(
      new PointerEvent('pointermove', {
        bubbles: true,
        clientX: -10,
        clientY: 2000,
        pointerType: 'mouse',
      }),
    );

    const msg = messages[0] as Extract<ClientMessage, { type: 'pointer'; eventType: 'pointermove' }>;
    expect(msg.pointer.x).toBe(0);
    expect(msg.pointer.y).toBe(1);
  });
});

// ─── Touch ghost-touch bug ────────────────────────────────────────────────────
//
// Root cause: normalizeTouches was dropping all touches with coordinates
// outside [0,1], including touchend / touchcancel events for fingers that
// slid off the canvas edge. The server never received those releases and kept
// phantom active touches. Those phantoms combined with the next real tap to
// look like multi-touch, causing spurious context menus (two-finger tap
// ≡ right-click in many apps) and broken single-click behavior on Android.

describe('touch ghost-touch: off-screen release must not be dropped', () => {
  let canvas: HTMLCanvasElement;
  let messages: ClientMessage[];
  let detach: () => void;

  beforeEach(() => {
    canvas = makeCanvas(1920, 1080);
    ({ messages, detach } = collectMessages(canvas));
  });

  afterEach(() => {
    detach();
    vi.restoreAllMocks();
  });

  it('sends touchend when the finger lifted off-screen (the ghost-touch bug)', () => {
    // Finger starts on the canvas…
    canvas.dispatchEvent(
      new TouchEvent('touchstart', {
        bubbles: true,
        cancelable: true,
        touches: [makeTouch(1, 500, 300, canvas)],
        changedTouches: [makeTouch(1, 500, 300, canvas)],
      }),
    );

    // …then slides off the top edge and lifts there.
    canvas.dispatchEvent(
      new TouchEvent('touchend', {
        bubbles: true,
        cancelable: true,
        touches: [] as Touch[],
        changedTouches: [makeTouch(1, 500, -80, canvas)], // clientY < 0 → off-screen
      }),
    );

    const touchMsgs = messages.filter((m) => m.type === 'touch');
    expect(touchMsgs).toHaveLength(2); // start + end, not just start

    const endMsg = touchMsgs[1] as Extract<ClientMessage, { type: 'touch' }>;
    expect(endMsg.eventType).toBe('touchend');
    expect(endMsg.touches[0].identifier).toBe(1);
    // y must be clamped to the canvas edge, not the raw negative value.
    expect(endMsg.touches[0].y).toBe(0);
    expect(endMsg.touches[0].x).toBeCloseTo(500 / 1920, 4);
  });

  it('sends touchcancel even when coordinates are off-screen', () => {
    canvas.dispatchEvent(
      new TouchEvent('touchstart', {
        bubbles: true,
        cancelable: true,
        touches: [makeTouch(1, 500, 300, canvas)],
        changedTouches: [makeTouch(1, 500, 300, canvas)],
      }),
    );

    canvas.dispatchEvent(
      new TouchEvent('touchcancel', {
        bubbles: true,
        cancelable: true,
        touches: [] as Touch[],
        changedTouches: [makeTouch(1, 500, -80, canvas)],
      }),
    );

    const touchMsgs = messages.filter((m) => m.type === 'touch');
    expect(touchMsgs).toHaveLength(2);
    expect((touchMsgs[1] as Extract<ClientMessage, { type: 'touch' }>).eventType).toBe('touchcancel');
  });

  it('clamps touchmove coordinates instead of dropping the event', () => {
    canvas.dispatchEvent(
      new TouchEvent('touchstart', {
        bubbles: true,
        cancelable: true,
        touches: [makeTouch(1, 500, 300, canvas)],
        changedTouches: [makeTouch(1, 500, 300, canvas)],
      }),
    );

    // Finger swipes past the right edge (clientX = 2500 > canvas width 1920).
    canvas.dispatchEvent(
      new TouchEvent('touchmove', {
        bubbles: true,
        cancelable: true,
        touches: [makeTouch(1, 2500, 300, canvas)],
        changedTouches: [makeTouch(1, 2500, 300, canvas)],
      }),
    );

    const touchMsgs = messages.filter((m) => m.type === 'touch');
    expect(touchMsgs).toHaveLength(2); // move must not be silently dropped

    const moveMsg = touchMsgs[1] as Extract<ClientMessage, { type: 'touch' }>;
    expect(moveMsg.eventType).toBe('touchmove');
    expect(moveMsg.touches[0].x).toBe(1); // clamped to right edge
    expect(moveMsg.touches[0].y).toBeCloseTo(300 / 1080, 4);
  });

  it('still drops a touchstart whose contact begins off-canvas', () => {
    canvas.dispatchEvent(
      new TouchEvent('touchstart', {
        bubbles: true,
        cancelable: true,
        touches: [makeTouch(1, -100, -100, canvas)],
        changedTouches: [makeTouch(1, -100, -100, canvas)],
      }),
    );

    expect(messages.filter((m) => m.type === 'touch')).toHaveLength(0);
  });
});
