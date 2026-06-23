<script lang="ts">
  interface Props {
    fullscreenTarget: () => HTMLElement | null;
  }
  let { fullscreenTarget }: Props = $props();

  let isFullscreen = $state(!!document.fullscreenElement);

  function onFullscreenChange(): void {
    isFullscreen = !!document.fullscreenElement;
  }

  $effect(() => {
    document.addEventListener('fullscreenchange', onFullscreenChange);
    return () => document.removeEventListener('fullscreenchange', onFullscreenChange);
  });

  // iPhone Safari has no element Fullscreen API at all (requestFullscreen
  // is undefined there), so this is silently a no-op on iOS rather than an
  // error. A web app manifest with `display: standalone` is the real
  // mitigation for that case, and is out of scope here.
  async function toggleFullscreen(): Promise<void> {
    if (document.fullscreenElement) {
      await document.exitFullscreen();
      return;
    }
    const target = fullscreenTarget();
    if (!target?.requestFullscreen) return;
    try {
      await target.requestFullscreen();
    } catch (e) {
      console.error('Fullscreen request failed:', e);
    }
  }
</script>

<button type="button" aria-pressed={isFullscreen} onclick={toggleFullscreen}>
  {isFullscreen ? 'Exit fullscreen' : 'Fullscreen'}
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
