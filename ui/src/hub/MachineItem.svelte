<script lang="ts">
  import { tick } from "svelte";
  import Dot from "../components/Dot.svelte";
  import { asHubFailure, hubFailureText } from "./api";
  import { hubName, hubSummary, incompleteReasons } from "./derive";
  import { hub } from "./store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";
  import type { HubMachine, HubMachines } from "./types";

  let { machine, data, onremoved }: { machine: HubMachine; data: HubMachines; onremoved: () => void } = $props();

  const uid = $props.id();
  const name = $derived(hubName(machine));
  const summary = $derived(hubSummary(machine, data, t, f, hub.now));
  const reasons = $derived(summary.kind === "incomplete" ? incompleteReasons(machine) : []);
  const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
  const when = (iso: string) => cap(t.at(f.moment(iso, hub.now)));
  const l = t.hub.list;

  // Removing: a confirmation that says exactly what happens, then the request.
  let asking = $state(false);
  let sending = $state(false);
  let problem = $state<string | null>(null);
  let askButton: HTMLButtonElement | undefined = $state();
  let keepButton: HTMLButtonElement | undefined = $state();
  let problemEl: HTMLElement | undefined = $state();

  async function ask() {
    asking = true;
    problem = null;
    await tick();
    keepButton?.focus();
  }

  async function keep() {
    if (sending) return;
    asking = false;
    await tick();
    askButton?.focus();
  }

  async function confirm() {
    if (sending) return;
    sending = true;
    problem = null;
    try {
      const r = await hub.remove(machine.machine_id);
      statusBar.show(r === "gone" ? t.hub.remove.gone : t.hub.remove.done(name));
      onremoved();
    } catch (e) {
      const fail = asHubFailure(e);
      if (fail.unauthorized) return;
      problem =
        fail.status === 503 || fail.code === "pbs_unreachable"
          ? t.hub.remove.pbs
          : fail.code === "pbs_refused"
            ? t.hub.remove.refused(hubFailureText(fail, t))
            : t.hub.remove.failed(hubFailureText(fail, t));
      asking = false;
      await tick();
      problemEl?.focus();
    } finally {
      sending = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape" && asking && !sending) {
      e.preventDefault();
      void keep();
    }
  }
</script>

<li class="m" aria-labelledby="{uid}-n">
  <div class="body">
    <h3 class="n" id="{uid}-n">{name}</h3>
    <p class="st" class:bad={summary.tone === "bad"}>
      <Dot tone={summary.tone === "ok" ? "neutral" : summary.tone} />{summary.lead}
    </p>
    <p class="tx">{summary.text}</p>
    {#if reasons.length}
      <ul class="reasons">
        {#each reasons as r, i (i)}<li>{r}</li>{/each}
      </ul>
    {/if}

    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div class="rm" onkeydown={onKey}>
      {#if asking}
        <div class="notice-box ask" role="group" aria-labelledby="{uid}-q" aria-describedby="{uid}-what">
          <p class="q" id="{uid}-q">{t.hub.remove.ask(name)}</p>
          <p class="what" id="{uid}-what">{t.hub.remove.what(name)}</p>
          <div class="ask-b">
            <button class="btn small" type="button" aria-disabled={sending ? "true" : undefined} onclick={confirm}>
              {sending ? t.hub.remove.working : t.hub.remove.confirm}
            </button>
            <button class="btn small primary" type="button" bind:this={keepButton} aria-disabled={sending ? "true" : undefined} onclick={keep}>
              {t.hub.remove.keep}
            </button>
          </div>
        </div>
      {:else}
        <button class="linkbtn small" type="button" bind:this={askButton} aria-label={l.removeLabel(name)} onclick={ask}>{l.remove}</button>
      {/if}
      {#if problem}
        <p class="hint bad" role="alert" tabindex="-1" bind:this={problemEl}>{problem}</p>
      {/if}
    </div>
  </div>

  <dl class="facts">
    <div>
      <dt>{l.last}</dt>
      <dd>
        <!-- Before the hub ever read the PBS, "never" would be a guess. -->
        {machine.last_snapshot ? when(machine.last_snapshot) : data.pbs_read_at ? l.lastNone : t.other.unknown}
        {#if machine.snapshot_count > 0}<span class="sub">{l.count(machine.snapshot_count)}</span>{/if}
      </dd>
    </div>
    {#if machine.last_snapshot}
      <div>
        <dt>{l.verified}</dt>
        <dd class:bad-text={machine.last_snapshot_verified === false}>
          {machine.last_snapshot_verified === true
            ? l.verifiedOk
            : machine.last_snapshot_verified === false
              ? l.verifiedBad
              : l.verifiedNone}
        </dd>
      </div>
    {/if}
    {#if machine.os}
      <div><dt>{l.system}</dt><dd>{machine.os}</dd></div>
    {/if}
    {#if machine.backup_id !== name}
      <div><dt>{l.backupId}</dt><dd><code>{machine.backup_id}</code></dd></div>
    {/if}
    <div><dt>{l.joined}</dt><dd>{f.dayMonth(machine.joined_at)}</dd></div>
    {#if machine.nestegg_version}
      <div><dt>{l.version}</dt><dd>{machine.nestegg_version}</dd></div>
    {/if}
  </dl>
</li>

<style>
  .m {
    display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 360px); gap: 12px 32px;
    padding: 16px 0 18px; border-bottom: 1px solid var(--rule);
  }
  .n { font-size: var(--fs-m); font-weight: 600; overflow-wrap: anywhere; }
  .st { display: flex; gap: 8px; align-items: baseline; margin-top: 4px; font-weight: 500; }
  .st :global(.dot) { transform: translateY(-1px); }
  .st.bad { color: var(--bad); }
  .tx { color: var(--muted); margin-top: 2px; max-width: 66ch; }
  .reasons { margin-top: 6px; padding-left: 18px; list-style: disc; color: var(--muted); }
  .reasons li { margin-top: 2px; max-width: 66ch; overflow-wrap: anywhere; }
  .rm { margin-top: 10px; }
  .rm .linkbtn { font-size: 13px; }
  .ask { max-width: 560px; }
  .q { font-weight: 600; }
  .what { color: var(--muted); margin-top: 4px; font-size: 13px; max-width: 66ch; }
  .ask-b { display: flex; gap: 8px; margin-top: 10px; flex-wrap: wrap; }
  .facts { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px 20px; align-content: start; font-size: 13px; }
  .facts > div { min-width: 0; }
  .facts dt { color: var(--faint); font-size: var(--fs-s); }
  .facts dd { overflow-wrap: anywhere; }
  .facts .sub { display: block; color: var(--muted); font-size: var(--fs-s); }
  .bad-text { color: var(--bad); }

  @media (max-width: 900px) {
    .m { grid-template-columns: minmax(0, 1fr); }
  }
  @media (max-width: 760px) {
    .rm { margin-top: 2px; }
    .rm .linkbtn { min-height: 44px; display: inline-flex; align-items: center; }
  }
</style>
