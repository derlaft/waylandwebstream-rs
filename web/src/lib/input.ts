// Touch/pointer/wheel input, normalized against the canvas's *live*
// bounding rect on every event (never a cached rect) so input stays
// aligned through resizes, rotation, and the side panel opening.
import type { ClientMessage, PointerPoint, TouchPoint } from './protocol';

export function attachInput(
  canvas: HTMLCanvasElement,
  sendControl: (msg: ClientMessage) => void,
): () => void {
  function normalizeTouches(touchList: TouchList): TouchPoint[] {
    const rect = canvas.getBoundingClientRect();
    const touches: TouchPoint[] = [];
    for (let i = 0; i < touchList.length; i++) {
      const touch = touchList[i];
      const x = (touch.clientX - rect.left) / rect.width;
      const y = (touch.clientY - rect.top) / rect.height;
      // Drop touches outside [0,1] -- a defensive clamp against
      // floating-point rect math at the exact edge, now that the canvas's
      // CSS box always matches the viewport exactly (see viewport.ts).
      if (x >= 0 && x <= 1 && y >= 0 && y <= 1) {
        touches.push({ identifier: touch.identifier, x, y, pressure: touch.force || 0.5 });
      }
    }
    return touches;
  }

  function sendTouch(
    eventType: 'touchstart' | 'touchmove' | 'touchend' | 'touchcancel',
    touchList: TouchList,
  ): void {
    const touches = normalizeTouches(touchList);
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
    const rect = canvas.getBoundingClientRect();
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
  const onPointerDown = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
    e.preventDefault();
    canvas.setPointerCapture(e.pointerId);
    // Clicking the stream should be enough to start sending keys, without
    // a separate click target -- requires the canvas to be focusable (see
    // the `tabindex` on the <canvas> in Stage.svelte).
    canvas.focus();
    sendControl({ type: 'pointer', eventType: 'pointerdown', pointer: normalizedPointer(e) });
  };
  const onPointerMove = (e: PointerEvent): void => {
    if (e.pointerType === 'touch') return;
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

  const onWheel = (e: WheelEvent): void => {
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const x = Math.min(Math.max((e.clientX - rect.left) / rect.width, 0), 1);
    const y = Math.min(Math.max((e.clientY - rect.top) / rect.height, 0), 1);
    sendControl({ type: 'pointer', eventType: 'wheel', x, y, deltaX: e.deltaX, deltaY: e.deltaY });
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

  return () => {
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
  };
}
