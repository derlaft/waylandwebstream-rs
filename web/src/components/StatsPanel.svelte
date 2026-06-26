<script lang="ts">
  import { streamStats } from '../lib/stats';
</script>

{#if $streamStats.cursorDebug !== null}
  {@const c = $streamStats.cursorDebug}
  <details open class="cursor-debug">
    <summary>Cursor debug</summary>
    <dl class="stats">
      <dt>Messages</dt><dd>{c.count}</dd>
      <dt>Last kind</dt><dd>{c.kind}</dd>
      <dt>Overlay display</dt><dd>{c.overlayDisplay || '(empty)'}</dd>
      <dt>Overlay size</dt><dd>{c.imgW}×{c.imgH}</dd>
      <dt>Transform</dt><dd class="mono">{c.overlayTransform || '(empty)'}</dd>
    </dl>
  </details>
{/if}

<dl class="stats">
  <dt>Connection</dt>
  <dd>{$streamStats.connectionState}</dd>

  <dt>End-to-end latency</dt>
  <dd>{$streamStats.endToEndLatencyMs.toFixed(1)} ms</dd>

  <dt>Bitrate</dt>
  <dd>
    {$streamStats.bitrateBps > 0
      ? `${($streamStats.bitrateBps / 1_000_000).toFixed(2)} Mbps`
      : '—'}
  </dd>

  <dt>Resolution</dt>
  <dd>
    {$streamStats.resolution
      ? `${$streamStats.resolution.width}x${$streamStats.resolution.height}`
      : '—'}
  </dd>

  <dt>Arrival gap p95 / max</dt>
  <dd>{$streamStats.arrivalGapP95Ms.toFixed(1)} / {$streamStats.arrivalGapMaxMs.toFixed(1)} ms</dd>

  <dt>Bursts</dt>
  <dd>{$streamStats.burstCount}</dd>

  <dt>Max decode queue</dt>
  <dd>{$streamStats.maxDecodeQueue}</dd>

  <dt>Decode avg</dt>
  <dd>{$streamStats.decodeAvgMs.toFixed(1)} ms</dd>

  <dt>Blit avg / p95</dt>
  <dd>{$streamStats.blitAvgMs.toFixed(1)} / {$streamStats.blitP95Ms.toFixed(1)} ms</dd>
</dl>

<style>
  .stats {
    display: grid;
    grid-template-columns: auto auto;
    column-gap: 12px;
    row-gap: 6px;
    margin: 0;
    font-size: 13px;
  }

  dt {
    color: #999;
    grid-column: 1;
  }

  dd {
    margin: 0;
    grid-column: 2;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .cursor-debug {
    margin-bottom: 12px;
  }

  .cursor-debug summary {
    font-size: 12px;
    color: #aaa;
    cursor: pointer;
    margin-bottom: 6px;
  }

  .mono {
    font-family: monospace;
    font-size: 11px;
    word-break: break-all;
  }
</style>
