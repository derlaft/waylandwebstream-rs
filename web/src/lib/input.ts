// Touch/pointer/wheel input, normalized against the canvas's bounding rect so
// input stays aligned through resizes, rotation, and scroll. The rect is read
// from a ResizeObserver-backed cache (see lib/rectCache.ts) rather than live on
// every event: pointermove/wheel fire 60-120x/sec and each live read forces a
// synchronous layout on the same canvas the decoder paints to. The cache is
// refreshed exactly when the box changes, so coordinates stay just as accurate.
import type { ClientMessage, PointerPoint, TouchPoint } from './protocol';
import { observeRect } from './rectCache';

export interface InputHandle {
  /// Tear down all listeners.
  detach: () => void;
  /// Arm/disarm browser Pointer Lock (relative motion), driven by the server's
  /// `pointer_lock` message. Disarming also exits an active lock.
  setPointerLockWanted: (wanted: boolean) => void;
}

export function attachInput(
  canvas: HTMLCanvasElement,
  sendControl: (msg: ClientMessage) => void,
): InputHandle {
  const rectCache = observeRect(canvas);
  // Converts a TouchList to normalized [0,1] coordinates.
  //
  // `clampOutOfBounds = false` (touchstart only): drops contacts whose
  // coordinates are outside the canvas. A new contact starting off the
  // canvas should not be tracked at all.
  //
  // `clampOutOfBounds = true` (touchmove / touchend / touchcancel): clamps
  // instead of dropping. A finger that slid off the screen still needs to
  // send its release event; if the touchend is dropped the server is left
  // with a phantom active touch that never gets released. Phantom touches
  // combine with real ones to look like multi-touch to the remote app --
  // that is what causes spurious context menus on the next single tap.
  function normalizeTouches(touchList: TouchList, clampOutOfBounds: boolean): TouchPoint[] {
    const rect = rectCache.get();
    const vv = window.visualViewport;
    const w = vv && vv.width > 0 ? vv.width : rect.width;
    const h = vv && vv.height > 0 ? vv.height : rect.height;
    const touches: TouchPoint[] = [];
    for (let i = 0; i < touchList.length; i++) {
      const touch = touchList[i];
      const x = (touch.clientX - rect.left) / w;
      const y = (touch.clientY - rect.top) / h;
      if (clampOutOfBounds) {
        touches.push({
          identifier: touch.identifier,
          x: Math.min(Math.max(x, 0), 1),
          y: Math.min(Math.max(y, 0), 1),
          pressure: touch.force || 0.5,
        });
      } else if (x >= 0 && x <= 1 && y >= 0 && y <= 1) {
        touches.push({ identifier: touch.identifier, x, y, pressure: touch.force || 0.5 });
      }
    }
    return touches;
  }

  function sendTouch(
    eventType: 'touchstart' | 'touchmove' | 'touchend' | 'touchcancel',
    touchList: TouchList,
  ): void {
    const clamp = eventType !== 'touchstart';
    const touches = normalizeTouches(touchList, clamp);
    if (touches.length > 0) {
      sendControl({ type: 'touch', eventType, touches });
    }
  }

  const onTouchStart = (e: TouchEvent): void => {
    e.preventDefault();
    sendTouch('touchstart', e.touches);
  };
  const onTouchMove = (e: TouchEvent): void => {
    e.preventDefault();
    sendTouch('touchmove', e.touches);
  };
  const onTouchEnd = (e: TouchEvent): void => {
    e.preventDefault();
    sendTouch('touchend', e.changedTouches);
  };
  const onTouchCancel = (e: TouchEvent): void => {
    e.preventDefault();
    sendTouch('touchcancel', e.changedTouches);
  };

  canvas.addEventListener('touchstart', onTouchStart, { passive: false });
  canvas.addEventListener('touchmove', onTouchMove, { passive: false });
  canvas.addEventListener('touchend', onTouchEnd, { passive: false });
  canvas.addEventListener('touchcancel', onTouchCancel, { passive: false });

  function normalizedPointer(e: PointerEvent): PointerPoint {
    const rect = rectCache.get();
    const x = (e.clientX - rect.left) / rect.width;
    const y = (e.clientY - rect.top) / rect.height;
    return {
      x: Math.min(Math.max(x, 0), 1),
      y: Math.min(Math.max(y, 0), 1),
      button: e.button,
      pointerType: e.pointerType,
      pressure: e.pressure,
    };
  }

  // Touch contacts also arrive here as PointerEvents (pointerType
  // "touch"), but those are already handled above via the dedicated touch
  // listeners, so they're ignored here to avoid injecting the same
  // physical contact twice.
  // Set true while a remote client holds a pointer lock (see setPointerLockWanted,
  // driven by the server's `pointer_lock` message). Pointer Lock can only be
  // requested from within a user-gesture handler, so we arm it here and request
  // it on the next pointerdown.
  let pointerLockWanted = false;

  const onPointerDown = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
    e.preventDefault();
    canvas.setPointerCapture(e.pointerId);
    // Clicking the stream should be enough to start sending keys, without
    // a separate click target -- requires the canvas to be focusable (see
    // the `tabindex` on the <canvas> in Stage.svelte).
    canvas.focus();
    // A client wants the pointer captured (e.g. an FPS game): use this gesture
    // to enter Pointer Lock. The button event still goes through below so the
    // click reaches the app.
    if (pointerLockWanted && document.pointerLockElement !== canvas) {
      canvas.requestPointerLock?.();
    }
    sendControl({ type: 'pointer', eventType: 'pointerdown', pointer: normalizedPointer(e) });
  };
  const onPointerMove = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
    // While locked the browser reports no absolute position, only deltas. Send
    // them as relative motion, pre-scaled from CSS px to compositor/output px
    // (canvas.width is the decoded frame = output resolution).
    if (document.pointerLockElement === canvas) {
      const rect = rectCache.get();
      const scaleX = rect.width > 0 ? canvas.width / rect.width : 1;
      const scaleY = rect.height > 0 ? canvas.height / rect.height : 1;
      sendControl({
        type: 'pointer',
        eventType: 'pointerrelative',
        dx: e.movementX * scaleX,
        dy: e.movementY * scaleY,
      });
      return;
    }
    sendControl({ type: 'pointer', eventType: 'pointermove', pointer: normalizedPointer(e) });
  };
  const onPointerUp = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
    canvas.releasePointerCapture(e.pointerId);
    sendControl({ type: 'pointer', eventType: 'pointerup', pointer: normalizedPointer(e) });
  };
  const onPointerCancel = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
    sendControl({ type: 'pointer', eventType: 'pointercancel', pointer: normalizedPointer(e) });
  };

  canvas.addEventListener('pointerdown', onPointerDown);
  canvas.addEventListener('pointermove', onPointerMove);
  canvas.addEventListener('pointerup', onPointerUp);
  canvas.addEventListener('pointercancel', onPointerCancel);

  // Right-click should reach the remote app instead of opening the
  // browser's own context menu.
  const onContextMenu = (e: Event): void => e.preventDefault();
  canvas.addEventListener('contextmenu', onContextMenu);

  // Browsers report wheel deltas in three different units depending on the
  // device and browser. Firefox physical mouse wheels use DOM_DELTA_LINE
  // (deltaY ≈ 3), while Chrome uses DOM_DELTA_PIXEL (deltaY ≈ 100) for the
  // same physical click. Passing raw values through would make scrolling
  // behave completely differently across browsers. Normalize everything to
  // CSS pixels so the server always sees one consistent unit.
  const LINE_HEIGHT_PX = 40; // 1 line ≈ 40px; 3 Firefox lines ≈ 120px ≈ Chrome's ~100px
  function normalizedWheelDelta(e: WheelEvent, canvasHeight: number): { deltaX: number; deltaY: number } {
    if (e.deltaMode === WheelEvent.DOM_DELTA_LINE) {
      return { deltaX: e.deltaX * LINE_HEIGHT_PX, deltaY: e.deltaY * LINE_HEIGHT_PX };
    }
    if (e.deltaMode === WheelEvent.DOM_DELTA_PAGE) {
      return { deltaX: e.deltaX * canvasHeight, deltaY: e.deltaY * canvasHeight };
    }
    return { deltaX: e.deltaX, deltaY: e.deltaY };
  }

  const onWheel = (e: WheelEvent): void => {
    e.preventDefault();
    const rect = rectCache.get();
    const x = Math.min(Math.max((e.clientX - rect.left) / rect.width, 0), 1);
    const y = Math.min(Math.max((e.clientY - rect.top) / rect.height, 0), 1);
    const { deltaX, deltaY } = normalizedWheelDelta(e, rect.height);
    sendControl({ type: 'pointer', eventType: 'wheel', x, y, deltaX, deltaY });
  };
  canvas.addEventListener('wheel', onWheel, { passive: false });

  // Keyboard: forwards `KeyboardEvent.code` (physical key identity), not
  // `.key` (layout-resolved character) -- see KeyMessage in protocol.ts.
  // `e.repeat` keydowns are dropped so the server's own Wayland repeat-rate
  // config is the single source of key repeat, instead of compounding with
  // the browser's own auto-repeat.
  //
  // `pressedCodes` tracks every code we've sent a keydown for but no
  // matching keyup yet. This is needed because a combo like Alt+Tab gets
  // intercepted by the OS/window manager and switches focus *away* from the
  // browser -- the keyup for Alt (or Tab) then fires in whatever app the OS
  // switched to, never reaching this page at all. Without releasing
  // everything still in this set on blur, the server would consider that
  // key held forever, silently modifying every keystroke after it.
  const pressedCodes = new Set<string>();

  const releaseAllKeys = (): void => {
    for (const code of pressedCodes) {
      sendControl({ type: 'key', eventType: 'keyup', code });
    }
    pressedCodes.clear();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    e.preventDefault();
    if (e.repeat) return;
    pressedCodes.add(e.code);
    sendControl({ type: 'key', eventType: 'keydown', code: e.code });
  };
  const onKeyUp = (e: KeyboardEvent): void => {
    e.preventDefault();
    pressedCodes.delete(e.code);
    sendControl({ type: 'key', eventType: 'keyup', code: e.code });
  };
  canvas.addEventListener('keydown', onKeyDown);
  canvas.addEventListener('keyup', onKeyUp);
  // `blur` covers the canvas losing focus without the window losing it
  // (e.g. Tab-navigating to the side panel); `window.blur` covers the whole
  // browser window losing OS focus (e.g. Alt+Tab, clicking another app);
  // `visibilitychange` is a backstop for cases neither fires (some
  // OS/browser combinations on tab switch or minimize).
  canvas.addEventListener('blur', releaseAllKeys);
  window.addEventListener('blur', releaseAllKeys);
  const onVisibilityChange = (): void => {
    if (document.visibilityState === 'hidden') releaseAllKeys();
  };
  document.addEventListener('visibilitychange', onVisibilityChange);

  const setPointerLockWanted = (wanted: boolean): void => {
    pointerLockWanted = wanted;
    // Server says the lock was released -> exit if the browser still holds it.
    if (!wanted && document.pointerLockElement === canvas) {
      document.exitPointerLock?.();
    }
  };

  const detach = (): void => {
    canvas.removeEventListener('touchstart', onTouchStart);
    canvas.removeEventListener('touchmove', onTouchMove);
    canvas.removeEventListener('touchend', onTouchEnd);
    canvas.removeEventListener('touchcancel', onTouchCancel);
    canvas.removeEventListener('pointerdown', onPointerDown);
    canvas.removeEventListener('pointermove', onPointerMove);
    canvas.removeEventListener('pointerup', onPointerUp);
    canvas.removeEventListener('pointercancel', onPointerCancel);
    canvas.removeEventListener('contextmenu', onContextMenu);
    canvas.removeEventListener('wheel', onWheel);
    canvas.removeEventListener('keydown', onKeyDown);
    canvas.removeEventListener('keyup', onKeyUp);
    canvas.removeEventListener('blur', releaseAllKeys);
    window.removeEventListener('blur', releaseAllKeys);
    document.removeEventListener('visibilitychange', onVisibilityChange);
    if (document.pointerLockElement === canvas) document.exitPointerLock?.();
    rectCache.dispose();
  };

  return { detach, setPointerLockWanted };
}
