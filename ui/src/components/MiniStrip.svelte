<script lang="ts">
  import type { DayCell } from "../lib/derive";

  /** The 14 days as small ticks next to a name. The status line next to it says the same in words. */
  let { cells }: { cells: DayCell[] } = $props();
</script>

<span class="ticks" aria-hidden="true">
  {#each cells as c (c.key)}<i class="k-{c.kind}"></i>{/each}
</span>

<style>
  .ticks { display: flex; gap: 2px; align-items: flex-end; height: 13px; }
  i { display: block; width: 4px; height: 13px; border-radius: 1px; background: var(--tick); }
  .k-none { height: 3px; }
  .k-pbs { background: var(--pbs); }
  .k-fail { background: var(--bad); }
  .k-incomplete { background: var(--warn); }
  /* Damaged: hollow with a red base; running: solid blue. Not told apart by colour alone. */
  .k-damaged {
    background: linear-gradient(var(--bad), var(--bad)) bottom / 100% 5px no-repeat;
    box-shadow: inset 0 0 0 1px var(--bad);
  }
  .k-run { background: var(--accent-text); }
  /* Not understood: hollow with a dashed-looking grey base. */
  .k-unknown {
    background: linear-gradient(var(--muted), var(--muted)) bottom / 100% 5px no-repeat;
    box-shadow: inset 0 0 0 1px var(--muted);
  }
</style>
