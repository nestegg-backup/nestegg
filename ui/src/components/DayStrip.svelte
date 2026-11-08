<script lang="ts">
  import type { DayCell, DayKind } from "../lib/derive";
  import { f, t } from "../lib/locale";

  let { cells }: { cells: DayCell[] } = $props();

  const label = (k: DayKind) => t.strip[k];
  const order: DayKind[] = ["ok", "none", "run", "fail", "pbs"];
  const used = $derived(order.filter((k) => cells.some((c) => c.kind === k)));
</script>

<ol class="strip" aria-label={t.strip.title}>
  {#each cells as c (c.key)}
    <li class="night" class:today={c.today} title="{f.keyShort(c.key)}: {label(c.kind)}">
      <span class="mark m-{c.kind}"><i></i></span>
      <span aria-hidden="true">{f.keyNumber(c.key)}</span>
      <span class="sr">{f.keyLong(c.key)}: {label(c.kind)}</span>
    </li>
  {/each}
</ol>
<div class="legend" aria-hidden="true">
  {#each used as k (k)}
    <span><span class="mark m-{k}"><i></i></span>{label(k)}</span>
  {/each}
</div>

<style>
  .strip { display: grid; grid-template-columns: repeat(14, minmax(0, 1fr)); gap: 4px; max-width: 560px; }
  .night { display: flex; flex-direction: column; align-items: center; gap: 6px; }
  .mark { width: 100%; max-width: 26px; height: 30px; border-radius: 4px; display: flex; align-items: flex-end; }
  .mark i { display: block; width: 100%; height: 100%; border-radius: 4px; }
  .m-ok i { background: var(--mark); }
  .m-none i { box-shadow: inset 0 0 0 1.5px var(--mark); }
  .m-fail i { background: var(--bad); }
  .m-pbs i { background: var(--pbs); }
  .m-run i { box-shadow: inset 0 0 0 1.5px var(--accent); }
  .night > span:not(.mark) { font-size: var(--fs-s); color: var(--faint); line-height: 1; }
  .night.today > span:not(.mark) { color: var(--text); }
  .legend { display: flex; flex-wrap: wrap; gap: 6px 20px; margin-top: 16px; font-size: var(--fs-s); color: var(--muted); }
  .legend > span { display: inline-flex; align-items: center; gap: 8px; }
  .legend .mark { width: 10px; height: 14px; border-radius: 2px; }
  .legend .mark i { border-radius: 2px; }
  @media (max-width: 760px) {
    .strip { gap: 3px; }
  }
</style>
