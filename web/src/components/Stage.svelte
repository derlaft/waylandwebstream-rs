<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { AudioStream } from '../lib/audio';
  import { ControlChannel } from '../lib/control';
  import { attachInput } from '../lib/input';
  import type { ClientMessage, CursorUpdate } from '../lib/protocol';
  import { setCursorDebug } from '../lib/stats';
  import { VideoStream } from '../lib/stream';
  import { Viewport } from '../lib/viewport';

  let canvas: HTMLCanvasElement;
  let cursorOverlay: HTMLImageElement;

  // Hotspot state for the cursor overlay.
  // Updated by applyCursor; read by the pointermove handler closure.
  let cursorHotX = 0;
  let cursorHotY = 0;
  let cursorSurfaceActive = false;
  let cursorMsgCount = 0;

  let control: ControlChannel | null = null;
  let stream: VideoStream | null = null;
  let audio: AudioStream | null = null;
  let viewport: Viewport | null = null;
  let detachInput: (() => void) | null = null;
  let removeCursorListeners: (() => void) | null = null;

  function sendControl(msg: ClientMessage): void {
    control?.send(msg);
  }

  // Applies a compositor cursor update. For surface cursors the RGBA pixels
  // are drawn into an off-screen canvas and exported as a PNG data URL, then
  // set as the src of the overlay <img>. The overlay follows the OS pointer
  // via a CSS transform updated in the pointermove handler, so the OS cursor
  // itself is always hidden (cursor:none on both canvas and .stage).
  function applyCursor(update: CursorUpdate): void {
    cursorMsgCount++;
    if (update.kind === 'surface') {
      canvas.style.cursor = 'none';
      const raw = atob(update.rgba);
      const bytes = new Uint8ClampedArray(raw.length);
      for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
      const oc = document.createElement('canvas');
      oc.width = update.width;
      oc.height = update.height;
      oc.getContext('2d')!.putImageData(new ImageData(bytes, update.width, update.height), 0, 0);
      cursorOverlay.src = oc.toDataURL();
      cursorOverlay.style.width = `${update.width}px`;
      cursorOverlay.style.height = `${update.height}px`;
      cursorHotX = update.hotspot_x;
      cursorHotY = update.hotspot_y;
      cursorSurfaceActive = true;
      // If the pointer is already over the canvas (stationary mouse case),
      // show immediately. Otherwise pointermove will show it on first move.
      if (canvas.matches(':hover')) cursorOverlay.style.display = 'block';
    } else {
      cursorSurfaceActive = false;
      cursorOverlay.style.display = 'none';
      // named  → native cursor with the requested CSS name
      // hidden → app explicitly hid the cursor
      // default → hide as well: labwc renders cursor in its own frame via
      //   software compositing (wlr_scene), so it never calls set_cursor on
      //   the parent; the cursor IS visible in the video stream. Showing the
      //   native browser cursor on top just creates a double-cursor, and
      //   hiding it lets GIMP/labwc cursor-hide requests (nil set_cursor) work
      //   correctly because the browser cursor is already gone.
      if (update.kind === 'named') {
        canvas.style.cursor = update.name;
      } else {
        canvas.style.cursor = 'none';
      }
    }
    setCursorDebug({
      kind: update.kind,
      count: cursorMsgCount,
      overlayDisplay: cursorOverlay.style.display,
      overlayTransform: cursorOverlay.style.transform,
      imgW: cursorOverlay.offsetWidth,
      imgH: cursorOverlay.offsetHeight,
    });
  }

  function teardown(): void {
    removeCursorListeners?.();
    removeCursorListeners = null;
    detachInput?.();
    detachInput = null;
    viewport?.stop();
    viewport = null;
    stream?.close();
    stream = null;
    audio?.close();
    audio = null;
    control?.close();
    control = null;
  }

  onMount(() => {
    control = new ControlChannel({
      onCodec: (codec) => stream?.setCodec(codec),
      onCursor: applyCursor,
    });
    control.connect();

    stream = new VideoStream({ canvas, sendControl });
    stream.connect();

    audio = new AudioStream();
    audio.connect();

    viewport = new Viewport({ canvas, sendControl });
    viewport.start();

    detachInput = attachInput(canvas, sendControl);

    // Keyboard events only reach a focused element, and there's no other
    // focusable content on the page competing for it -- so focus the canvas
    // immediately rather than requiring a click first (clicking still
    // re-focuses it too, e.g. after the side panel steals focus; see
    // input.ts's pointerdown handler).
    canvas.focus();

    const onMove = (e: PointerEvent) => {
      if (e.pointerType === 'touch') return;
      const rect = canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;
      cursorOverlay.style.transform = `translate(${x - cursorHotX}px, ${y - cursorHotY}px)`;
      // Show on first move in case pointerenter was missed (e.g. mouse was
      // already over the canvas when the cursor surface was set).
      if (cursorSurfaceActive) cursorOverlay.style.display = 'block';
    };
    const onLeave = (e: PointerEvent) => {
      if (e.pointerType === 'touch') return;
      cursorOverlay.style.display = 'none';
    };

    canvas.addEventListener('pointermove', onMove);
    canvas.addEventListener('pointerleave', onLeave);
    removeCursorListeners = () => {
      canvas.removeEventListener('pointermove', onMove);
      canvas.removeEventListener('pointerleave', onLeave);
    };

    window.addEventListener('beforeunload', teardown);
  });

  onDestroy(() => {
    window.removeEventListener('beforeunload', teardown);
    teardown();
  });
</script>

<div class="stage">
  <canvas bind:this={canvas} tabindex="0"></canvas>
  <!-- Cursor overlay: always cursor:none on the stage; this <img> renders the
       app cursor as a DOM element that follows the OS pointer via transform.
       pointer-events:none ensures it doesn't swallow mouse events. -->
  <img
    bind:this={cursorOverlay}
    class="cursor-overlay"
    alt=""
    draggable="false"
    src="data:,"
    style="display: none; transform: translate(0, 0);"
  />
</div>

<style>
  .stage {
    position: relative;
    width: 100%;
    height: 100%;
    background: #000;
    overflow: hidden;
  }

  canvas {
    position: absolute;
    top: 0;
    left: 0;
    touch-action: none;
    user-select: none;
    -webkit-user-select: none;
    -webkit-touch-callout: none;
    outline: none;
  }

  .cursor-overlay {
    position: absolute;
    top: 0;
    left: 0;
    pointer-events: none;
    user-select: none;
    image-rendering: pixelated;
    will-change: transform;
  }
</style>
