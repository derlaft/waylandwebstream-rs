<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { AudioStream } from '../lib/audio';
  import { ControlChannel } from '../lib/control';
  import { attachInput } from '../lib/input';
  import type { ClientMessage, CursorUpdate } from '../lib/protocol';
  import { VideoStream } from '../lib/stream';
  import { Viewport } from '../lib/viewport';

  let canvas: HTMLCanvasElement;
  // CSS cursor value applied to the canvas element. Starts hidden; updated
  // whenever the compositor sends a cursor change over the control channel.
  let cursorCss = 'none';

  let control: ControlChannel | null = null;
  let stream: VideoStream | null = null;
  let audio: AudioStream | null = null;
  let viewport: Viewport | null = null;
  let detachInput: (() => void) | null = null;

  function sendControl(msg: ClientMessage): void {
    control?.send(msg);
  }

  /// Converts a compositor CursorUpdate to a CSS cursor string. For surface
  /// cursors the pixels are drawn into an off-screen canvas and exported as
  /// a data URL so the browser renders them without a network round-trip.
  function applyCursor(update: CursorUpdate): void {
    switch (update.kind) {
      case 'default':
        cursorCss = 'none';
        break;
      case 'hidden':
        cursorCss = 'none';
        break;
      case 'named':
        cursorCss = update.name;
        break;
      case 'surface': {
        const raw = atob(update.rgba);
        const bytes = new Uint8ClampedArray(raw.length);
        for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
        const imgData = new ImageData(bytes, update.width, update.height);
        const offscreen = document.createElement('canvas');
        offscreen.width = update.width;
        offscreen.height = update.height;
        offscreen.getContext('2d')!.putImageData(imgData, 0, 0);
        cursorCss = `url(${offscreen.toDataURL()}) ${update.hotspot_x} ${update.hotspot_y}, auto`;
        break;
      }
    }
  }

  function teardown(): void {
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

    window.addEventListener('beforeunload', teardown);
  });

  onDestroy(() => {
    window.removeEventListener('beforeunload', teardown);
    teardown();
  });
</script>

<div class="stage">
  <canvas bind:this={canvas} tabindex="0" style="cursor: {cursorCss}"></canvas>
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
</style>
