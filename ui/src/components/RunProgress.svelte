<script lang="ts">
  import { app } from "../lib/store.svelte";
  import { runStep } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Run } from "../lib/types";

  /** `stale`: the daemon does not answer; the steps show the last known state. */
  let { run, wakeOnLan, stale = false }: { run: Run; wakeOnLan: boolean; stale?: boolean } = $props();

  const now = $derived(runStep(run.state));
  const elapsed = $derived(Math.max(0, (app.now.getTime() - new Date(run.started_at).getTime()) / 1000));

  // Archives finished so far against the size of the last backup: coarse (it moves after each archive), but honest.
  const bar = $derived(
    now === 2 && run.expected_bytes && run.processed_bytes != null && run.expected_bytes > 0
      ? { value: Math.min(run.processed_bytes, run.expected_bytes), max: run.expected_bytes }
      : null,
  );

  const steps = $derived([
    {
      title: wakeOnLan ? t.self.stepWake : t.self.stepReach,
      sub:
        now === 0
          ? run.state === "starting"
            ? t.self.stepStartingNow
            : wakeOnLan
              ? t.self.stepWakeNow
              : t.self.stepReachNow
          : t.self.stepWakeDone,
    },
    { title: t.self.stepInventory, sub: now === 1 ? t.self.stepInventorySub : "" },
    {
      title: t.self.stepUpload,
      sub:
        now === 2
          ? run.uploaded_bytes
            ? t.self.stepUploadSub(f.size(run.uploaded_bytes))
            : t.self.stepUploadCheck
          : now > 2 && run.uploaded_bytes
            ? t.self.stepUploadSub(f.size(run.uploaded_bytes))
            : "",
    },
    { title: t.self.stepFinish, sub: now === 3 ? t.self.stepFinishSub : "" },
  ]);
</script>

<h2 class="lead"><span class="pulse" aria-hidden="true"></span>{t.self.runningLead}</h2>
<p class="lead-text">{t.self.runningText}</p>
<ol class="steps">
  {#each steps as s, i (i)}
    <li class="step" class:done={i < now} class:now={i === now} aria-current={i === now ? "step" : undefined}>
      <span class="n" aria-hidden="true">{i + 1}</span>
      <div>
        {s.title}
        {#if s.sub}<div class="sub">{s.sub}</div>{/if}
        {#if i === 2 && bar}
          <progress class="bar" max={bar.max} value={bar.value} aria-label={t.self.stepUploadProgress(f.size(bar.value), f.size(bar.max))}></progress>
          <div class="sub">{t.self.stepUploadProgress(f.size(bar.value), f.size(bar.max))}</div>
        {/if}
      </div>
    </li>
  {/each}
</ol>
<p class="hint">{stale ? t.conn.runUnknown : t.self.runningSince(f.duration(elapsed), run.trigger !== "timer")}</p>

<style>
  .pulse { width: 10px; height: 10px; border-radius: 50%; background: var(--accent-text); flex: none; transform: translateY(-2px); animation: pulse 1.6s ease-in-out infinite; }
  @keyframes pulse { 50% { opacity: 0.35; } }
  .steps { margin-top: 16px; display: grid; gap: 10px; }
  .step { display: grid; grid-template-columns: 22px minmax(0, 1fr); gap: 12px; align-items: start; color: var(--faint); }
  .n {
    width: 22px; height: 22px; border-radius: 50%; display: grid; place-items: center;
    font-size: var(--fs-s); box-shadow: inset 0 0 0 1px var(--field-border); margin-top: 0;
  }
  .step.now { color: var(--text); font-weight: 500; }
  .step.now .n { box-shadow: inset 0 0 0 2px var(--accent-text); }
  .step.done { color: var(--muted); }
  .step.done .n { background: var(--tick); box-shadow: none; color: var(--on-tick); }
  .bar { display: block; width: 100%; max-width: 360px; height: 6px; margin-top: 6px; accent-color: var(--accent); }
  .sub { font-size: var(--fs-s); color: var(--muted); margin-top: 1px; font-weight: 400; }
  .hint { margin-top: 14px; }
</style>
