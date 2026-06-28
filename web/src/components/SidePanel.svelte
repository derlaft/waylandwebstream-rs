<script lang="ts">
  import { onMount } from 'svelte';
  import FullscreenButton from './FullscreenButton.svelte';
  import NativeResolutionButton from './NativeResolutionButton.svelte';
  import StatsPanel from './StatsPanel.svelte';

  interface Props {
    fullscreenTarget: () => HTMLElement | null;
  }
  let { fullscreenTarget }: Props = $props();

  let open = $state(false);
  let panelEl: HTMLDivElement | undefined = $state();
  let tabEl: HTMLButtonElement | undefined = $state();

  function close(): void {
    if (!open) return;
    open = false;
    // Return keyboard focus to the stream canvas so remote input works
    // immediately. Focusing tabEl here would steal focus from the canvas
    // (e.g. when Escape is pressed while the canvas had focus), breaking
    // keyboard input to the remote app until the user clicks the canvas again.
    document.querySelector<HTMLCanvasElement>('canvas')?.focus();
  }

  function toggle(): void {
    if (open) {
      close();
    } else {
      open = true;
    }
  }

  // Capture-phase so this runs before canvas input handlers (input.ts) --
  // an outside tap while the panel is open should only dismiss the panel,
  // not also act as a real pointer input forwarded to the remote desktop.
  function onPointerDownCapture(e: PointerEvent): void {
    if (open && panelEl && !panelEl.contains(e.target as Node)) {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  }

  function onKeydown(e: KeyboardEvent): void {
    if (e.key === 'Escape') close();
  }

  onMount(() => {
    window.addEventListener('pointerdown', onPointerDownCapture, true);
    window.addEventListener('keydown', onKeydown);
    return () => {
      window.removeEventListener('pointerdown', onPointerDownCapture, true);
      window.removeEventListener('keydown', onKeydown);
    };
  });
</script>

<div class="panel" class:open bind:this={panelEl}>
  <button
    class="tab"
    type="button"
    bind:this={tabEl}
    aria-expanded={open}
    aria-controls="side-panel-content"
    aria-label={open ? 'Close settings panel' : 'Open settings panel'}
    onclick={toggle}
  >
    {open ? '›' : '‹'}
  </button>
  <div class="content" id="side-panel-content" inert={!open}>
    <FullscreenButton {fullscreenTarget} />
    <NativeResolutionButton />
    <StatsPanel />
  </div>
</div>

<style>
  .panel {
    position: fixed;
    top: 0;
    right: 0;
    height: 100%;
    width: 280px;
    z-index: 10;
    display: flex;
    align-items: flex-start;
    transform: translateX(calc(100% - 32px));
    transition: transform 0.2s ease-out;
    /* The flex container's own box spans the full screen height even
       though only the 48px-tall .tab button is ever drawn in it -- without
       this, that empty space (the whole right edge of the screen, full
       height, 32px wide once collapsed) would still intercept pointer/touch
       events meant for the canvas underneath. .tab and .content opt back
       into hit-testing individually below. */
    pointer-events: none;
  }

  .panel.open {
    transform: translateX(0);
  }

  .tab {
    flex: 0 0 32px;
    width: 32px;
    height: 48px;
    margin-top: 16px;
    border: none;
    border-radius: 6px 0 0 6px;
    background: rgba(0, 0, 0, 0.6);
    color: #fff;
    font-size: 18px;
    line-height: 1;
    cursor: pointer;
    pointer-events: auto;
  }

  .content {
    flex: 1 1 auto;
    height: 100%;
    box-sizing: border-box;
    padding: 16px 12px;
    background: rgba(20, 20, 20, 0.92);
    color: #fff;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 16px;
    pointer-events: auto;
  }
</style>
