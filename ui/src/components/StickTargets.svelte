<script lang="ts">
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { restore } from "../lib/restore.svelte";
  import { href } from "../lib/router.svelte";
  import { reveal } from "../lib/reveal";
  import { canBackUpToStick, hasPbs, selfKind, selfTone, serverBackups, stickOnly, stickTone, stickFillPercent } from "../lib/derive";
  import { selfRowText, stickLine } from "../lib/text";
  import { f, t } from "../lib/locale";
  import type { Status, TargetStatus } from "../lib/types";

  /**
   * The backup destinations of this PC: the server (when there is one) and the stick. One row each,
   * like the rows of the settings; colour only on problems. With a server, the stick row has its own
   * "Back up to stick now"; with the stick alone that is the page's main button instead.
   */
  let { status, stick }: { status: Status; stick: TargetStatus } = $props();

  const line = $derived(stickLine(stick, t, f, app.now, restore.usingStick));
  const tone = $derived(stickTone(line.view.kind));
  const server = $derived(hasPbs(status));
  // The legacy fields say "running" for a run to either target: the server line uses the server's own state.
  const pbsView = $derived.by((): Status => {
    const p = status.targets?.pbs;
    if (!p) return status;
    return { ...status, health: p.health, current_run: p.current_run ?? null, last_run: p.last_run ?? null, last_success: p.last_success ?? null };
  });
  const serverTone = $derived(selfTone(selfKind(pbsView)));
  const serverText = $derived(selfRowText(pbsView, t, f, app.now, serverBackups(app.snapshots), false));
  const offerHere = $derived(!stickOnly(status) && canBackUpToStick(line.view, app.running));
  const lastSuccess = $derived(stick.last_success?.started_at ?? null);
  const damaged = $derived(line.view.damaged);
  const fill = $derived(stickFillPercent(stick));
</script>

<section class="targets" aria-labelledby="h-targets">
  <h2 class="sec-title" id="h-targets">{t.stick.title}</h2>
  <div class="box">
    {#if server}
      <div class="trow">
        <div class="txt">
          <p class="l">{t.stick.server} <span class="where">{status.pbs.server}</span></p>
          <p class="h" class:bad={serverTone === "bad"} class:warn={serverTone === "warn"}>
            {#if serverTone === "bad" || serverTone === "warn"}<Dot tone={serverTone} />{/if}{serverText}
          </p>
        </div>
        <div class="c"><a class="linkbtn" href={href({ view: "server" })}>{t.stick.toServer}</a></div>
      </div>
    {/if}
    <div class="trow">
      <div class="txt">
        <p class="l">{t.stick.name}</p>
        <p class="h state" class:bad={tone === "bad"} class:warn={tone === "warn"}>
          {#if tone === "bad" || tone === "warn"}<Dot {tone} />{/if}{line.lead}
        </p>
        {#if line.plug}<p class="h">{line.plug}</p>{/if}
        <!-- Stick only: the page's status sentence above already explains it. -->
        {#if line.why && !stickOnly(status)}<p class="h why">{line.why}</p>{/if}
        <p class="facts">
          {#if lastSuccess && line.view.kind !== "ok"}<span>{t.stick.lastSuccess(f.moment(lastSuccess, app.now))}</span>{/if}
          {#if stick.available_bytes != null}<span>{[t.stick.free(f.size(stick.available_bytes)), fill != null ? t.stick.filled(fill) : null].filter(Boolean).join(" · ")} ({t.stick.freeNote})</span>{/if}
          {#if stick.full_verified_at}<span>{t.stick.fullCheck(f.dayMonth(stick.full_verified_at))}</span>{/if}
          <span>{t.stick.overdueAfter(stick.overdue_after_hours)}</span>
        </p>
        {#if damaged}<p class="h bad">{t.stick.damagedSnapshots(damaged)}</p>{/if}
        {#if app.stickStartError && !stickOnly(status)}
          <p class="h" class:bad={app.stickStartError.kind !== "conflict"} role="status" use:reveal>
            {app.stickStartError.kind === "conflict" ? t.self.runConflict : t.stick.runFailed(app.stickStartError.message)}
          </p>
        {/if}
      </div>
      {#if offerHere}
        <div class="c">
          <button class="btn" type="button" disabled={app.stickStarting || app.offline} onclick={() => app.startStickRun()}>
            {app.stickStarting ? t.stick.starting : t.stick.runNow}
          </button>
        </div>
      {/if}
    </div>
  </div>
</section>

<style>
  .targets { margin-top: 22px; max-width: 720px; }
  .trow {
    display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 4px 20px; align-items: center;
    padding: 12px 14px; border-top: 1px solid var(--rule);
  }
  .trow:first-child { border-top: 0; }
  .txt { min-width: 0; }
  .l { font-weight: 500; }
  .where { color: var(--muted); font-weight: 400; margin-left: 6px; overflow-wrap: anywhere; }
  .h { color: var(--muted); font-size: 13px; margin-top: 2px; max-width: 66ch; display: flex; gap: 8px; align-items: baseline; }
  .h :global(.dot) { transform: translateY(-1px); }
  .h.state { color: var(--text); }
  .h.bad { color: var(--bad); }
  .h.warn { color: var(--warn); }
  .why { display: block; }
  .facts { display: flex; flex-wrap: wrap; gap: 2px 14px; margin-top: 6px; color: var(--faint); font-size: var(--fs-s); }
  .c { display: flex; align-items: center; justify-self: end; }
  @media (max-width: 760px) {
    .trow { grid-template-columns: minmax(0, 1fr); }
    .c { justify-self: start; }
  }
</style>
