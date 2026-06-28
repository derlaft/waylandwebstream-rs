<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { ClientMessage } from '../lib/protocol';
  import { attachSoftKeyboard, fabPosition } from '../lib/softKeyboard';

  interface Props {
    /// Forwards a translated key message to the server (Stage's sendControl).
    sendControl: (msg: ClientMessage) => void;
    /// Called when the button is tapped, so the stage can reconnect if the
    /// connection had dropped (typing alone doesn't touch the canvas, which
    /// is what normally triggers reconnect).
    onActivate?: () => void;
  }
  let { sendControl, onActivate }: Props = $props();

  // The hidden field that owns the device's native soft keyboard, and the
  // floating button that summons it.
  let inputEl: HTMLTextAreaElement | undefined = $state();
  let fabEl: HTMLDivElement | undefined = $state();

  const FAB_SIZE = 52; // px; keep in sync with the .fab CSS box
  const EDGE_GAP = 12; // px gap from the viewport edge when clamping
  const DRAG_THRESHOLD = 6; // px of movement before a press counts as a drag

  // Button position in CSS px from the top-left. Seeded from the persisted
  // store, defaulted to the bottom-right corner on first use.
  let pos = $state<{ x: number; y: number }>({ x: 0, y: 0 });
  // Whether the soft keyboard is currently up (the hidden field is focused).
  let active = $state(false);

  // Drag bookkeeping for the current press.
  let dragging = false;
  let moved = false;
  let startX = 0;
  let startY = 0;
  let startPosX = 0;
  let startPosY = 0;
  // Whether the keyboard was already up when this press started. Captured at
  // pointerdown because tapping can blur the field before pointerup fires --
  // reading the live focus state there would make a close-tap reopen.
  let wasActiveAtStart = false;

  function clamp(p: { x: number; y: number }): { x: number; y: number } {
    const maxX = Math.max(EDGE_GAP, window.innerWidth - FAB_SIZE - EDGE_GAP);
    const maxY = Math.max(EDGE_GAP, window.innerHeight - FAB_SIZE - EDGE_GAP);
    return {
      x: Math.min(Math.max(p.x, EDGE_GAP), maxX),
      y: Math.min(Math.max(p.y, EDGE_GAP), maxY),
    };
  }

  function toggleKeyboard(): void {
    if (wasActiveAtStart) {
      inputEl?.blur();
    } else {
      onActivate?.();
      // focus() inside the pointerup gesture is what opens the native
      // keyboard on mobile.
      inputEl?.focus();
    }
  }

  function onPointerDown(e: PointerEvent): void {
    e.stopPropagation();
    // Prevent the FAB from taking focus: otherwise the post-tap focus shift
    // would blur the hidden field and close the keyboard the instant it
    // opened. (The element is also a non-focusable <div role="button">.)
    e.preventDefault();
    dragging = true;
    moved = false;
    wasActiveAtStart = active;
    startX = e.clientX;
    startY = e.clientY;
    startPosX = pos.x;
    startPosY = pos.y;
    fabEl?.setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent): void {
    if (!dragging) return;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;
    if (!moved && Math.hypot(dx, dy) < DRAG_THRESHOLD) return;
    moved = true;
    pos = clamp({ x: startPosX + dx, y: startPosY + dy });
  }

  function onPointerUp(e: PointerEvent): void {
    if (!dragging) return;
    dragging = false;
    fabEl?.releasePointerCapture(e.pointerId);
    if (moved) {
      fabPosition.set(pos); // persist the new resting place
    } else {
      toggleKeyboard(); // a tap (no real movement) opens/closes the keyboard
    }
  }

  let detach: (() => void) | null = null;

  onMount(() => {
    // Seed position from the store, or default to the bottom-right corner.
    const saved = $fabPosition;
    pos = clamp(
      saved ?? {
        x: window.innerWidth - FAB_SIZE - EDGE_GAP,
        y: window.innerHeight - FAB_SIZE - EDGE_GAP,
      },
    );

    // `autocorrect` is a non-standard WebKit attribute (no Svelte type), so
    // set it imperatively to keep iOS Safari from rewriting typed text.
    inputEl?.setAttribute('autocorrect', 'off');

    if (inputEl) detach = attachSoftKeyboard(inputEl, sendControl);

    // Keep `active` in sync when the keyboard is dismissed by the system
    // (e.g. Android back button) rather than by tapping the button.
    const onFocus = () => (active = true);
    const onBlur = () => (active = false);
    inputEl?.addEventListener('focus', onFocus);
    inputEl?.addEventListener('blur', onBlur);

    // Re-clamp into view if the viewport shrinks (rotation, soft keyboard).
    const onResize = () => (pos = clamp(pos));
    window.addEventListener('resize', onResize);

    return () => {
      inputEl?.removeEventListener('focus', onFocus);
      inputEl?.removeEventListener('blur', onBlur);
      window.removeEventListener('resize', onResize);
    };
  });

  onDestroy(() => {
    detach?.();
    detach = null;
  });
</script>

<!-- Hidden, focusable field that owns the native soft keyboard. Kept empty;
     softKeyboard.ts translates its beforeinput/key events into remote keys.
     NOT aria-hidden: Chrome blocks (and instantly removes) focus on an
     aria-hidden element, which would close the keyboard the moment it opens. -->
<textarea
  bind:this={inputEl}
  class="osk-input"
  rows="1"
  inputmode="text"
  autocapitalize="off"
  autocomplete="off"
  spellcheck="false"
  tabindex="-1"
></textarea>

<!-- Non-focusable on purpose (a <div>, not a <button>): a focusable control
     would grab focus on tap and blur the hidden field. It's a touch FAB, so
     keyboard reachability isn't expected. -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  bind:this={fabEl}
  class="fab"
  class:active
  role="button"
  aria-label={active ? 'Hide on-screen keyboard' : 'Show on-screen keyboard'}
  aria-pressed={active}
  style="left: {pos.x}px; top: {pos.y}px;"
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={onPointerUp}
  onpointercancel={onPointerUp}
>
  ⌨
</div>

<style>
  .osk-input {
    position: absolute;
    top: 0;
    left: 0;
    width: 1px;
    height: 1px;
    padding: 0;
    border: 0;
    opacity: 0;
    resize: none;
    overflow: hidden;
    /* Don't let the off-screen field swallow taps meant for the canvas. */
    pointer-events: none;
  }

  .fab {
    position: absolute;
    width: 52px;
    height: 52px;
    z-index: 20;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    border-radius: 50%;
    background: rgba(0, 0, 0, 0.6);
    color: #fff;
    font-size: 24px;
    line-height: 1;
    cursor: pointer;
    /* Drag with one finger without the browser panning/zooming. */
    touch-action: none;
    user-select: none;
    -webkit-user-select: none;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
  }

  .fab.active {
    background: rgba(40, 110, 220, 0.85);
  }
</style>
