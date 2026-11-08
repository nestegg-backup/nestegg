<script lang="ts">
  let {
    backHref,
    backLabel,
    who,
    onback,
  }: { backHref?: string; backLabel?: string; who?: string; onback?: (e: MouseEvent) => void } = $props();
</script>

<div class="topline">
  {#if backHref}<a href={backHref} onclick={onback}>{backLabel}</a>{:else}<span></span>{/if}
  {#if who}<span class="who">{who}</span>{/if}
</div>

<style>
  .topline { display: flex; justify-content: space-between; align-items: center; margin-bottom: 36px; gap: 16px; }
  a { color: var(--muted); text-decoration: none; padding: 6px 0; }
  a:hover { color: var(--text); }
  .who { font-size: var(--fs-s); color: var(--faint); }
  @media (max-width: 760px) {
    .topline { margin-bottom: 26px; }
  }
</style>
