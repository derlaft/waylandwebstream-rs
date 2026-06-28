<script lang="ts">
  import { onScreenKeyboardEnabled } from '../lib/softKeyboard';

  // Shows/hides the floating on-screen-keyboard button on the stage. Off by
  // default; mainly useful on touch devices, which otherwise have no way to
  // summon the browser's native soft keyboard over the bare <canvas>.
  function toggle(): void {
    onScreenKeyboardEnabled.update((v) => !v);
  }
</script>

<button type="button" aria-pressed={$onScreenKeyboardEnabled} onclick={toggle}>
  {$onScreenKeyboardEnabled ? 'On-screen keyboard: on' : 'On-screen keyboard: off'}
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
