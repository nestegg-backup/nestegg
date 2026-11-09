<script lang="ts">
  import { untrack } from "svelte";
  import Cmd from "../Cmd.svelte";
  import TreeItem from "./TreeItem.svelte";
  import { app } from "../../lib/store.svelte";
  import { restore } from "../../lib/restore.svelte";
  import { href } from "../../lib/router.svelte";
  import { allDamaged, isDamaged } from "../../lib/derive";
  import { f, t } from "../../lib/locale";

  const snap = $derived(restore.snapshot(app.snapshots.list));

  $effect(() => {
    if (snap) untrack(() => void restore.prepare(snap, app.config.value));
  });

  const damaged = $derived(!!snap && isDamaged(snap, app.status?.server_verify_failed ?? []));
  const everyDamaged = $derived(allDamaged(app.snapshots.list, app.status?.server_verify_failed ?? []));
  const total = $derived(restore.totalSize());
  const n = $derived(restore.selection.length);
</script>

{#if snap}
  <p class="lead-text gap">
    {t.restore.snapshotOf(f.dayLongYear(snap.time), f.time(snap.time))}
    <a class="linkbtn" href={href({ view: "restore", step: "1" })}>{t.restore.otherSnapshot}</a>
  </p>
  {#if damaged}<p class="hint bad" role="status">{everyDamaged ? t.restore.damagedAll : t.restore.damagedSnapshot}</p>{/if}

  {#if restore.browse === "not_implemented"}
    <p class="lead-text gap">{t.restore.notAvailable}</p>
    <Cmd command="sudo nestegg restore {snap.snapshot} {restore.roots[0]?.archive ?? 'home.pxar'} {t.restore.notAvailableTarget}" />
  {:else}
    <ul class="tree box" aria-label={t.restore.treeLabel}>
      {#each restore.roots as node (node.archive)}
        <TreeItem {node} depth={0} />
      {/each}
    </ul>
    <div class="selbar">
      <span class="muted" aria-live="polite">
        {n ? t.restore.selected(n, total != null ? f.size(total) : null) : t.restore.nothingSelected}
      </span>
      <a class="btn primary" href={href({ view: "restore", step: "3" })} aria-disabled={n ? undefined : "true"} tabindex={n ? undefined : -1}>
        {t.restore.toTarget}
      </a>
    </div>
  {/if}
{/if}

<style>
  .gap { margin-top: 20px; }
  .tree { margin-top: 14px; padding: 6px; }
  .linkbtn { margin-left: 4px; }
</style>
