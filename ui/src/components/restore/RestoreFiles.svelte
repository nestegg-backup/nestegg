<script lang="ts">
  import { untrack } from "svelte";
  import Cmd from "../Cmd.svelte";
  import TreeItem from "./TreeItem.svelte";
  import { app } from "../../lib/store.svelte";
  import { restore } from "../../lib/restore.svelte";
  import { href } from "../../lib/router.svelte";
  import { allDamaged, isDamaged, stickView } from "../../lib/derive";
  import { stickProblemText } from "../../lib/text";
  import { f, t } from "../../lib/locale";

  const fromStick = $derived(restore.source === "stick");
  const snap = $derived(restore.snapshot(restore.snaps(app.snapshots).list));
  const stickTs = $derived(app.status?.targets?.stick ?? null);
  const plugged = $derived(stickTs ? stickView(stickTs).plugged : null);

  $effect(() => {
    if (snap) untrack(() => void restore.prepare(snap, app.config.value));
  });

  const failed = $derived((fromStick ? stickTs?.server_verify_failed : app.status?.server_verify_failed) ?? []);
  const damaged = $derived(!!snap && isDamaged(snap, failed));
  const everyDamaged = $derived(allDamaged(restore.snaps(app.snapshots).list, failed));
  // The stick went away (the status says so) or the last request to it failed.
  const stickNote = $derived(
    !fromStick ? null : plugged === false ? t.restore.stickGoneBrowse : restore.stickProblem ? stickProblemText(restore.stickProblem, t) : null,
  );
  const total = $derived(restore.totalSize());
  const n = $derived(restore.selection.length);
</script>

{#if snap}
  <p class="lead-text gap">
    {t.restore.snapshotOf(f.dayLongYear(snap.time), f.time(snap.time))}
    <a class="linkbtn" href={href({ view: "restore", step: "1" })}>{t.restore.otherSnapshot}</a>
  </p>
  {#if damaged}
    <p class="hint bad" role="status">
      {fromStick
        ? everyDamaged ? t.restore.damagedAllStick : t.restore.damagedSnapshotStick
        : everyDamaged ? t.restore.damagedAll : t.restore.damagedSnapshot}
    </p>
  {/if}
  {#if stickNote}
    <p class="hint bad" role="status">{stickNote}</p>
    {#if plugged !== false && restore.stickProblem?.message}<p class="hint">{t.errors.detail(restore.stickProblem.message)}</p>{/if}
  {/if}

  {#if restore.browse === "not_implemented" && fromStick}
    <p class="lead-text gap">{t.restore.stickNotSupported}</p>
  {:else if restore.browse === "not_implemented"}
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
