<script lang="ts">
  import { clipboardSyncEnabled } from '../lib/clipboard';

  // Syncs the clipboard between this device and the remote desktop. On by
  // default. Reading the device clipboard prompts for permission the first
  // time it's used (browser requirement); see lib/clipboard.ts.
  function toggle(): void {
    clipboardSyncEnabled.update((v) => !v);
  }
</script>

<button type="button" aria-pressed={$clipboardSyncEnabled} onclick={toggle}>
  {$clipboardSyncEnabled ? 'Clipboard sync: on' : 'Clipboard sync: off'}
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
