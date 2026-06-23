<script lang="ts">
  import Stage from './components/Stage.svelte';
  import SidePanel from './components/SidePanel.svelte';

  let container: HTMLDivElement | undefined = $state();
</script>

<div class="app-container" bind:this={container}>
  <Stage />
  <SidePanel fullscreenTarget={() => container ?? null} />
</div>

<style>
  .app-container {
    position: relative;
    width: 100%;
    height: 100%;
  }
</style>
