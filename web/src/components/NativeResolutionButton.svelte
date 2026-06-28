<script lang="ts">
  import { nativeResolution } from '../lib/viewport';

  // Toggles native device-pixel rendering. Off by default: the stream is
  // rendered in CSS px and upscaled by the browser (softer on HiDPI but
  // cheaper to encode). On: render at devicePixelRatio for a crisp,
  // native-resolution image at the cost of more pixels to encode/send.
  function toggle(): void {
    nativeResolution.update((v) => !v);
  }
</script>

<button type="button" aria-pressed={$nativeResolution} onclick={toggle}>
  {$nativeResolution ? 'Native resolution: on' : 'Native resolution: off'}
</button>

<style>
  button {
    width: 100%;
    padding: 8px 12px;
    background: #2a2a2a;
    color: #fff;
    border: 1px solid #444;
    border-radius: 4px;
    font-size: 13px;
    cursor: pointer;
  }

  button:hover {
    background: #3a3a3a;
  }
</style>
