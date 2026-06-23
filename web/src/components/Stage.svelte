<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { ControlChannel } from '../lib/control';
  import { attachInput } from '../lib/input';
  import type { ClientMessage } from '../lib/protocol';
  import { VideoStream } from '../lib/stream';
  import { Viewport } from '../lib/viewport';

  let canvas: HTMLCanvasElement;

  let control: ControlChannel | null = null;
  let stream: VideoStream | null = null;
  let viewport: Viewport | null = null;
  let detachInput: (() => void) | null = null;

  function sendControl(msg: ClientMessage): void {
    control?.send(msg);
  }

  function teardown(): void {
    detachInput?.();
    detachInput = null;
    viewport?.stop();
    viewport = null;
    stream?.close();
    stream = null;
    control?.close();
    control = null;
  }

  onMount(() => {
    control = new ControlChannel({ onCodec: (codec) => stream?.setCodec(codec) });
    control.connect();

    stream = new VideoStream({ canvas, sendControl });
    stream.connect();

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
  <canvas bind:this={canvas} tabindex="0"></canvas>
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
