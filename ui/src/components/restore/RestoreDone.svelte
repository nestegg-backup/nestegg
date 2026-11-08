<script lang="ts">
  import { app } from "../../lib/store.svelte";
  import { restore } from "../../lib/restore.svelte";
  import { href, router } from "../../lib/router.svelte";
  import { f, t } from "../../lib/locale";
  import type { Status } from "../../lib/types";

  let { status }: { status: Status } = $props();

  const job = $derived(restore.job);
  const target = $derived(job?.target_dir ?? "");
  const snapTime = $derived(restore.snapshotTime);
  const elapsed = $derived(
    job?.started_at ? Math.max(0, (app.now.getTime() - new Date(job.started_at).getTime()) / 1000) : 0,
  );

  function finish(e: MouseEvent) {
    e.preventDefault();
    restore.reset();
    router.go({ view: "self" });
  }
</script>

<div class="body" aria-live="polite">
  {#if job?.state === "running" && restore.jobUnknown}
    <p class="lead">{t.conn.lost}</p>
    <p class="lead-text">{t.conn.restoreUnknown}</p>
    <p class="hint">{t.restore.runningTarget} <code>{target}</code></p>
    <div class="actions"><button class="btn" type="button" onclick={() => restore.watch(0)}>{t.app.retry}</button></div>
  {:else if job?.state === "running"}
    <p class="lead">{t.restore.running}</p>
    <p class="hint">{t.restore.runningTarget} <code>{target}</code>{#if job.started_at}. {t.restore.runningSince(f.duration(elapsed))}{/if}</p>
  {:else if job?.state === "succeeded"}
    <p class="lead">{t.restore.done} <code>{target}</code>.</p>
    <p class="lead-text">{t.restore.doneText(restore.jobSelection.length, snapTime ? f.dayLong(snapTime) : "")}</p>
    <div class="actions">
      <a class="btn primary" href={href({ view: "self" })} onclick={finish}>{t.restore.backTo(status.machine.hostname)}</a>
    </div>
  {:else if job?.state === "failed"}
    <p class="lead">{t.restore.failed}</p>
    {#if job.error}<p class="lead-text">{job.error}</p>{/if}
    <div class="actions">
      <a class="btn primary" href={href({ view: "restore", step: "3" })}>{t.restore.tryAgain}</a>
      <a class="btn" href={href({ view: "self" })} onclick={finish}>{t.restore.backTo(status.machine.hostname)}</a>
    </div>
  {/if}
</div>

<style>
  .body { margin-top: 26px; }
  .lead { margin-top: 0; }
  .lead code { font-size: 0.85em; display: inline-block; max-width: 100%; overflow-wrap: anywhere; }
</style>
