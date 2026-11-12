<script lang="ts">
  import { untrack } from "svelte";
  import RestorePick from "./RestorePick.svelte";
  import RestoreFiles from "./RestoreFiles.svelte";
  import RestoreTarget from "./RestoreTarget.svelte";
  import RestoreDone from "./RestoreDone.svelte";
  import { app } from "../../lib/store.svelte";
  import { restore } from "../../lib/restore.svelte";
  import { href, router } from "../../lib/router.svelte";
  import { hasPbs, hasStick, stickView } from "../../lib/derive";
  import { t } from "../../lib/locale";
  import type { Status } from "../../lib/types";

  let { status, step }: { status: Status; step: "1" | "2" | "3" | "done" } = $props();

  const n = $derived(step === "done" ? 0 : Number(step));

  const pbs = $derived(hasPbs(status));
  const stick = $derived(hasStick(status));
  const plugged = $derived(status.targets?.stick ? stickView(status.targets.stick).plugged : null);

  // The source follows the targets: the stick alone means the stick, no stick means the server.
  // Before the steps' own effects, so they never start on the wrong source.
  $effect.pre(() => {
    const only = !pbs && stick ? "stick" : !stick ? "pbs" : null;
    if (only) untrack(() => restore.setSource(only));
  });

  $effect(() => {
    if (pbs) untrack(() => void app.loadSnapshots());
  });

  // Pulled out: its snapshots are read again from whichever stick is plugged in next.
  $effect(() => {
    if (plugged === false) untrack(() => restore.stickGone());
  });

  // Waiting for the stick: notice it soon after it is plugged in, not only at the next regular status poll.
  $effect(() => {
    if (restore.source !== "stick" || plugged !== false) return;
    const timer = setInterval(() => void app.poll(), 5_000);
    return () => clearInterval(timer);
  });

  // Steps need the earlier choices; fall back to the first step that is complete.
  $effect(() => {
    if (step === "done" && !restore.job) router.go({ view: "restore", step: "1" }, true);
    else if ((step === "2" || step === "3") && !restore.snapshotTime) router.go({ view: "restore", step: "1" }, true);
    else if (step === "3" && !restore.selection.length) router.go({ view: "restore", step: "2" }, true);
  });

  function cancel(e: MouseEvent) {
    e.preventDefault();
    restore.reset();
    router.go({ view: "self" });
  }
</script>

{#if n}
  <div class="head">
    <div class="status">
      <p class="stepline">{t.restore.stepOf(n)}</p>
      <h2 class="lead">{t.restore.stepTitles[n - 1] ?? ""}</h2>
    </div>
    <div class="acts">
      <a class="btn" href={href({ view: "self" })} onclick={cancel}>{t.restore.cancel}</a>
    </div>
  </div>
  <ol class="progress" aria-hidden="true">
    {#each t.restore.stepTitles as title, i (i)}
      <li class:done={i + 1 < n} class:now={i + 1 === n}>{title}</li>
    {/each}
  </ol>
{/if}

<div class="body">
  {#if step === "1"}
    <RestorePick {status} />
  {:else if step === "2"}
    <RestoreFiles />
  {:else if step === "3"}
    <RestoreTarget />
  {:else}
    <RestoreDone {status} />
  {/if}
</div>

<style>
  .stepline { font-size: var(--fs-s); color: var(--muted); margin-bottom: 2px; }
  .head { border-bottom: 0; padding-bottom: 14px; }
  .progress { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 4px; max-width: 720px; }
  .progress li {
    border-top: 3px solid var(--rule); padding-top: 6px; font-size: var(--fs-s); color: var(--faint);
    /* Long titles wrap onto a second line (phone, German): they are never cut off. */
    overflow-wrap: anywhere;
  }
  .progress li.done { border-color: var(--tick); color: var(--muted); }
  .progress li.now { border-color: var(--accent-text); color: var(--text); font-weight: 500; }
  .body { max-width: 720px; }
  @media (max-width: 760px) {
    /* "Abbrechen" stays a small button next to the step, not a full-width action. */
    .head { flex-wrap: nowrap; align-items: flex-start; }
    .head .status { flex: 1 1 auto; }
    .head .acts { width: auto; }
    .head .acts .btn { flex: none; padding: 7px 12px; }
  }
</style>
