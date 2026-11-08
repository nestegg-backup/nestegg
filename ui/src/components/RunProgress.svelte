<script lang="ts">
  import { app } from "../lib/store.svelte";
  import { runStep } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Run } from "../lib/types";

  /** `stale`: the daemon does not answer; the steps show the last known state. */
  let { run, wakeOnLan, stale = false }: { run: Run; wakeOnLan: boolean; stale?: boolean } = $props();

  const now = $derived(runStep(run.state));
  const elapsed = $derived(Math.max(0, (app.now.getTime() - new Date(run.started_at).getTime()) / 1000));

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

<p class="lead">{t.self.runningLead}</p>
<p class="lead-text">{t.self.runningText}</p>
<ol class="steps">
  {#each steps as s, i (i)}
    <li class="step" class:done={i < now} class:now={i === now} aria-current={i === now ? "step" : undefined}>
      <span class="n" aria-hidden="true">{i + 1}</span>
      <div>
        {s.title}
        {#if s.sub}<div class="sub">{s.sub}</div>{/if}
      </div>
    </li>
  {/each}
</ol>
{#if stale}
  <p class="hint">{t.conn.runUnknown}</p>
{:else}
  <p class="hint">{t.self.runningSince(f.duration(elapsed), run.trigger !== "timer")}</p>
{/if}

<style>
  .steps { margin-top: 26px; display: grid; gap: 14px; }
  .step { display: grid; grid-template-columns: 22px minmax(0, 1fr); gap: 12px; align-items: start; color: var(--faint); }
  .n {
    width: 22px; height: 22px; border-radius: 50%; display: grid; place-items: center;
    font-size: var(--fs-s); box-shadow: inset 0 0 0 1.5px var(--mark); margin-top: 1px;
  }
  .step.now { color: var(--text); }
  .step.now .n { box-shadow: inset 0 0 0 1.5px var(--text); }
  .step.done { color: var(--muted); }
  .step.done .n { background: var(--mark); box-shadow: none; color: var(--text); }
  .sub { font-size: var(--fs-s); color: var(--muted); margin-top: 2px; }
  .hint { margin-top: 22px; }
</style>
