<script lang="ts">
  import { untrack } from "svelte";
  import Topline from "../Topline.svelte";
  import RestorePick from "./RestorePick.svelte";
  import RestoreFiles from "./RestoreFiles.svelte";
  import RestoreTarget from "./RestoreTarget.svelte";
  import RestoreDone from "./RestoreDone.svelte";
  import { app } from "../../lib/store.svelte";
  import { restore } from "../../lib/restore.svelte";
  import { router } from "../../lib/router.svelte";
  import { t } from "../../lib/locale";
  import type { Status } from "../../lib/types";

  let { status, step }: { status: Status; step: "1" | "2" | "3" | "done" } = $props();

  const n = $derived(step === "done" ? 0 : Number(step));

  $effect(() => {
    untrack(() => void app.loadSnapshots());
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
  <Topline backHref="#/pc/self" backLabel={t.restore.cancel} onback={cancel} who={status.machine.hostname} />
{:else}
  <Topline who={status.machine.hostname} />
{/if}
<h1>{t.restore.title}</h1>
{#if n}<p class="stepline">{t.restore.step(n, t.restore.stepTitles[n - 1] ?? "")}</p>{/if}

{#if step === "1"}
  <RestorePick {status} />
{:else if step === "2"}
  <RestoreFiles />
{:else if step === "3"}
  <RestoreTarget />
{:else}
  <RestoreDone {status} />
{/if}

<style>
  .stepline { font-size: var(--fs-s); color: var(--muted); margin-top: 6px; }
</style>
