<script lang="ts">
  import type { DayCell, DayKind } from "../lib/derive";
  import { f, t } from "../lib/locale";

  let { cells }: { cells: DayCell[] } = $props();

  const label = (k: DayKind) => t.strip[k];
  const order: DayKind[] = ["ok", "incomplete", "none", "run", "pbs", "fail", "damaged", "unknown"];
  const used = $derived(order.filter((k) => cells.some((c) => c.kind === k)));
</script>

<h2 class="sec-title" id="h-days">{t.strip.title}</h2>
<ol class="cells" aria-labelledby="h-days">
  {#each cells as c (c.key)}
    <li class="k-{c.kind}" class:today={c.today} title="{f.keyShort(c.key)}: {label(c.kind)}">
      <span class="w" aria-hidden="true">{f.keyWeekday(c.key)}</span>
      <span class="d" aria-hidden="true">{f.keyNumber(c.key)}</span>
      <span class="sr">{f.keyLong(c.key)}: {label(c.kind)}</span>
    </li>
  {/each}
</ol>
<ul class="legend" aria-hidden="true">
  {#each used as k (k)}
    <li><i class="k-{k}"></i>{label(k)}</li>
  {/each}
</ul>

<style>
  .cells {
    display: grid; grid-template-columns: repeat(14, minmax(0, 1fr)); gap: 4px; max-width: 600px;
    margin-top: 18px;
  }
  .cells li {
    position: relative; height: 40px; border-radius: 5px; display: grid; place-items: center;
    font-size: var(--fs-s); font-weight: 500; background: var(--tick); color: var(--on-tick);
  }
  .w { position: absolute; top: -18px; font-size: var(--fs-xs); color: var(--faint); font-weight: 400; line-height: 1.3; }
  .cells .k-none { background: none; box-shadow: inset 0 0 0 1px var(--field-border); color: var(--muted); }
  .cells .k-pbs { background: var(--pbs); color: var(--on-pbs); }
  .cells .k-fail { background: var(--bad-fill); color: var(--on-bad-fill); }
  .cells .k-incomplete { background: var(--warn); color: var(--on-warn); }
  /* Damaged: dashed red ring. Running: solid blue ring. They must not differ by colour alone. */
  .cells .k-damaged { background: none; border: 2px dashed var(--bad); color: var(--text); }
  .cells .k-run { background: none; box-shadow: inset 0 0 0 2px var(--accent-text); color: var(--text); }
  /* Not understood: dashed neutral ring (neither a success nor a failure), apart from the red one by colour and by the hint in the legend. */
  .cells .k-unknown { background: none; border: 2px dashed var(--muted); color: var(--text); }
  /* The dashed ring is a border: keep the weekday where it is on the other days. */
  .cells .k-damaged .w, .cells .k-unknown .w { top: -20px; }
  .cells li.today { outline: 2px solid var(--text); outline-offset: 2px; }
  .cells li.today .w { color: var(--text); }

  .legend { display: flex; flex-wrap: wrap; gap: 4px 16px; margin-top: 12px; font-size: var(--fs-s); color: var(--muted); }
  .legend li { display: flex; align-items: center; gap: 6px; }
  .legend i { width: 12px; height: 12px; border-radius: 2px; background: var(--tick); }
  .legend .k-none { background: none; box-shadow: inset 0 0 0 1px var(--field-border); }
  .legend .k-pbs { background: var(--pbs); }
  .legend .k-fail { background: var(--bad-fill); }
  .legend .k-incomplete { background: var(--warn); }
  .legend .k-damaged { background: none; border: 2px dashed var(--bad); }
  .legend .k-run { background: none; box-shadow: inset 0 0 0 2px var(--accent-text); }
  .legend .k-unknown { background: none; border: 2px dashed var(--muted); }

  @media (max-width: 760px) {
    .cells { gap: 3px; }
    .cells li { height: 34px; font-size: 11.5px; border-radius: 4px; }
    .cells li.today { outline-offset: 1px; }
  }
</style>
