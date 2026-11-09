<script lang="ts">
  import Dot from "../Dot.svelte";
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

<div class="head solo" aria-live="polite">
  <div class="status">
    {#if job?.state === "running" && restore.jobUnknown}
      <h2 class="lead"><Dot tone="warn" />{t.conn.lost}</h2>
      <p class="lead-text">{t.conn.restoreUnknown}</p>
      <p class="hint">{t.restore.runningTarget} <code>{target}</code></p>
    {:else if job?.state === "running"}
      <h2 class="lead"><span class="pulse" aria-hidden="true"></span>{t.restore.running}</h2>
      <p class="lead-text">{t.restore.runningTarget} <code>{target}</code>{#if job.started_at}. {t.restore.runningSince(f.duration(elapsed))}{/if}</p>
      {#if job.restored_bytes}
        <p class="hint">
          {job.total_bytes ? t.restore.progress(f.size(job.restored_bytes), f.size(job.total_bytes)) : t.restore.progressDone(f.size(job.restored_bytes))}
        </p>
        {#if job.total_bytes}
          <progress class="bar" max={job.total_bytes} value={Math.min(job.restored_bytes, job.total_bytes)} aria-label={t.restore.running}></progress>
        {/if}
      {/if}
    {:else if job?.state === "succeeded"}
      <h2 class="lead">{t.restore.done}</h2>
      <p class="target"><code>{target}</code></p>
      <p class="lead-text">{t.restore.doneText(restore.jobSelection.length, snapTime ? f.dayLong(snapTime) : "")}</p>
      {#if job.warnings?.length}
        <p class="hint"><strong>{t.runs.warnings}:</strong> {job.warnings.join(" · ")}</p>
      {/if}
    {:else if job?.state === "failed"}
      <h2 class="lead bad"><Dot tone="bad" />{t.restore.failed}</h2>
      {#if job.error}<p class="lead-text">{job.error}</p>{/if}
    {/if}
  </div>
  <div class="acts">
    {#if job?.state === "running" && restore.jobUnknown}
      <button class="btn" type="button" onclick={() => restore.watch(0)}>{t.app.retry}</button>
    {:else if job?.state === "succeeded"}
      <a class="btn primary" href={href({ view: "self" })} onclick={finish}>{t.restore.backTo(status.machine.hostname)}</a>
    {:else if job?.state === "failed"}
      <a class="btn" href={href({ view: "self" })} onclick={finish}>{t.restore.backTo(status.machine.hostname)}</a>
      <a class="btn primary" href={href({ view: "restore", step: "3" })}>{t.restore.tryAgain}</a>
    {/if}
  </div>
</div>

<style>
  .bar { display: block; width: 100%; max-width: 420px; height: 6px; margin-top: 8px; accent-color: var(--accent); }
  .target { margin: 2px 0 8px; }
  .target code { font-size: 14px; color: var(--text); background: var(--box); border: 1px solid var(--rule); border-radius: 4px; padding: 2px 8px; overflow-wrap: anywhere; display: inline-block; max-width: 100%; }
  .pulse { width: 10px; height: 10px; border-radius: 50%; background: var(--accent-text); flex: none; transform: translateY(-2px); animation: pulse 1.6s ease-in-out infinite; }
  @keyframes pulse { 50% { opacity: 0.35; } }
</style>
