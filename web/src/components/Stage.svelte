<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { AudioStream } from '../lib/audio';
  import { ClientChannel } from '../lib/client';
  import { ClipboardBridge } from '../lib/clipboard';
  import { attachInput } from '../lib/input';
  import type { ClientMessage, CursorUpdate } from '../lib/protocol';
  import { onScreenKeyboardEnabled } from '../lib/softKeyboard';
  import { setCursorDebug, streamStats } from '../lib/stats';
  import { createVideoPipeline, type VideoPipeline } from '../lib/videoClient';
  import { Viewport } from '../lib/viewport';
  import OnScreenKeyboard from './OnScreenKeyboard.svelte';

  let canvas: HTMLCanvasElement;
  let cursorOverlay: HTMLImageElement;

  // Hotspot state for the cursor overlay.
  // Updated by applyCursor; read by the pointermove handler closure.
  let cursorHotX = 0;
  let cursorHotY = 0;
  let cursorSurfaceActive = false;
  let cursorMsgCount = 0;

  let client: ClientChannel | null = null;
  let clipboard: ClipboardBridge | null = null;
  let pipeline: VideoPipeline | null = null;
  let audio: AudioStream | null = null;
  let viewport: Viewport | null = null;
  let detachInput: (() => void) | null = null;
  let removeCursorListeners: (() => void) | null = null;
  // The video pipeline resolves asynchronously (it probes worker WebGL before
  // committing the canvas). If the component is torn down during that probe,
  // this flag tells the pending resolution to drop the pipeline it built.
  let disposed = false;

  function sendControl(msg: ClientMessage): void {
    client?.send(msg);
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
    disposed = true;
    removeCursorListeners?.();
    removeCursorListeners = null;
    detachInput?.();
    detachInput = null;
    clipboard?.destroy();
    clipboard = null;
    viewport?.stop();
    viewport = null;
    pipeline?.close();
    pipeline = null;
    audio?.close();
    audio = null;
    client?.close();
    client = null;
  }

  onMount(() => {
    audio = new AudioStream();
    audio.start();

    // Clipboard sync between the device and the remote desktop. Reads/writes
    // the device clipboard on user gestures (see lib/clipboard.ts). Images go
    // out via the channel's binary frame.
    clipboard = new ClipboardBridge(sendControl, (mime, bytes) =>
      client?.sendClipboardImage(mime, bytes),
    );

    // Construct the channel now (so the initial resize/ready buffer in its
    // send queue) but connect it only once the pipeline is ready -- see below.
    // Its callbacks reach `pipeline` lazily, and no video frames arrive until
    // connect(), so there's no window where a frame could be dropped.
    client = new ClientChannel({
      onCodec: (codec) => pipeline?.setCodec(codec),
      onCursor: applyCursor,
      onClipboard: (text) => clipboard?.onRemoteClipboard(text),
      onClipboardImage: (mime, bytes) => clipboard?.onRemoteImage(mime, bytes),
      onVideoFrame: (frame) => pipeline?.handleVideoFrame(frame),
      onAudioFrame: (frame) => audio?.handleAudioFrame(frame),
      onClosed: () => {
        // The server allows one client at a time and disabled auto-reconnect,
        // so a dropped/kicked connection stays closed. The overlay (bound to
        // connectionState) prompts the user; the interaction listeners below
        // reconnect on the next click/tap/keypress.
        console.info('Connection closed; click the canvas to reconnect.');
      },
    });

    viewport = new Viewport({ canvas, sendControl });
    viewport.start();

    // Building the pipeline is async: it may probe worker WebGL and, on the
    // worker path, call transferControlToOffscreen (after which the main
    // thread can't draw to the canvas -- only Viewport's CSS sizing and the
    // input listeners still touch it, which stay valid). Connect the channel
    // only after the decoder is up so the server's initial keyframe isn't
    // delivered into a not-yet-ready pipeline.
    createVideoPipeline({ canvas, sendControl }).then((p) => {
      if (disposed) {
        p.close();
        return;
      }
      pipeline = p;
      p.start();
      client?.connect();
    });

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

    // Reconnect on canvas interaction after a disconnect. reconnect() is a
    // no-op while the connection is open/connecting, so these can stay
    // attached for the lifetime of the component. Registered before
    // attachInput's listeners so the socket is already re-opening when the
    // same event is forwarded as input (it buffers until OPEN).
    const onReconnectIntent = () => {
      client?.reconnect();
      // A real user gesture: the moment we're allowed to read the device
      // clipboard and push it to the remote (see lib/clipboard.ts).
      void clipboard?.onUserGesture();
    };
    canvas.addEventListener('pointerdown', onReconnectIntent);
    canvas.addEventListener('touchstart', onReconnectIntent);
    canvas.addEventListener('keydown', onReconnectIntent);

    removeCursorListeners = () => {
      canvas.removeEventListener('pointermove', onMove);
      canvas.removeEventListener('pointerleave', onLeave);
      canvas.removeEventListener('pointerdown', onReconnectIntent);
      canvas.removeEventListener('touchstart', onReconnectIntent);
      canvas.removeEventListener('keydown', onReconnectIntent);
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
  {#if $onScreenKeyboardEnabled}
    <!-- Floating, draggable button that summons the device's native soft
         keyboard; its input is translated to remote keys (softKeyboard.ts).
         onActivate reconnects since typing never touches the canvas. -->
    <OnScreenKeyboard {sendControl} onActivate={() => client?.reconnect()} />
  {/if}
  {#if $streamStats.connectionState === 'closed'}
    <!-- pointer-events:none so the click/tap that dismisses this overlay
         reaches the canvas underneath, which triggers reconnect(). -->
    <div class="disconnected-overlay">
      <p>Disconnected</p>
      <p class="hint">Click, tap, or press a key to reconnect</p>
    </div>
  {/if}
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

  .disconnected-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    pointer-events: none;
    background: rgba(0, 0, 0, 0.6);
    color: #fff;
    font-family: system-ui, sans-serif;
    text-align: center;
  }

  .disconnected-overlay p {
    margin: 0;
    font-size: 1.5rem;
    font-weight: 600;
  }

  .disconnected-overlay .hint {
    font-size: 1rem;
    font-weight: 400;
    opacity: 0.8;
  }
</style>