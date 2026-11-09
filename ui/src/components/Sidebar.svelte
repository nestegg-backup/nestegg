<script lang="ts">
  import ConnectionNotice from "./ConnectionNotice.svelte";
  import Dot from "./Dot.svelte";
  import MiniStrip from "./MiniStrip.svelte";
  import { app } from "../lib/store.svelte";
  import { href, type Route } from "../lib/router.svelte";
  import {
    dayStrip,
    machineKind,
    machineName,
    machineSnapshotEvents,
    machineTone,
    selfKind,
    selfStrip,
    selfTone,
    serverBackups,
    sortMachines,
    type DayCell,
    type Tone,
  } from "../lib/derive";
  import { machineRowText, selfRowText, serverRow } from "../lib/text";
  import { f, t } from "../lib/locale";

  let { route }: { route: Route } = $props();

  const status = $derived(app.status);
  const selfSelected = $derived(route.view === "home" || route.view === "self" || route.view === "restore");
  const overdueHours = $derived(status?.overdue_after_hours);
  const others = $derived(
    app.machines && status ? sortMachines(app.machines.machines, status.machine.backup_id, app.now, overdueHours) : [],
  );
  const server = $derived(status ? serverRow(status, t, f, app.now) : null);
  // This PC: the same days as the boxes on its page, only once the history is loaded.
  const selfCells = $derived(
    status && status.health !== "unconfigured" && app.runs.state === "ok"
      ? selfStrip(app.runs.value, status.current_run, app.snapshots.list, app.now, f)
      : null,
  );
  // Other PCs: snapshot days from /machines. No strip without a current answer.
  const otherCells = (m: (typeof others)[number]): DayCell[] | null =>
    app.machinesError ? null : dayStrip(machineSnapshotEvents(m), app.now, f);
</script>

{#snippet row(link: string, current: boolean, name: string, tone: Tone | null, text: string | null, cells: DayCell[] | null)}
  <a class="row" href={link} aria-current={current ? "page" : undefined}>
    <span class="n">{name}</span>
    {#if cells}<MiniStrip {cells} />{:else}<span></span>{/if}
    {#if text !== null}
      <!-- Colour only for what needs attention: "all fine" keeps the plain grey dot. -->
      <span class="s" class:bad={tone === "bad"} class:warn={tone === "warn"}><Dot tone={tone === "ok" || !tone ? "neutral" : tone} />{text}</span>
    {/if}
  </a>
{/snippet}

<div class="side-in">
  {#if route.view === "home"}
    <!-- On a phone the list is a screen of its own and needs the notice too. -->
    <div class="phone-only"><ConnectionNotice /></div>
  {/if}
  {#if status}
    <h2 class="grp">{t.nav.thisPc}</h2>
    {@render row(
      href({ view: "self" }),
      selfSelected,
      status.machine.hostname,
      selfTone(selfKind(status)),
      selfRowText(status, t, f, app.now, serverBackups(app.snapshots)),
      selfCells,
    )}

    {#if others.length}
      <h2 class="grp">{t.nav.others}</h2>
      {#each others as m (m.backup_id)}
        {@render row(
          href({ view: "pc", id: m.backup_id }),
          route.view === "pc" && route.id === m.backup_id,
          machineName(m),
          machineTone(machineKind(m, app.now, overdueHours)),
          machineRowText(m, t, f, app.now, overdueHours),
          otherCells(m),
        )}
      {/each}
      <p class="note">{t.nav.othersNote}</p>
    {/if}
    {#if app.machinesError}
      {#if !others.length}<h2 class="grp">{t.nav.others}</h2>{/if}
      <p class="note">
        {t.load.othersFailed}
        <button class="linkbtn" type="button" onclick={() => app.loadMachines()}>{t.app.retry}</button>
      </p>
    {/if}

    <div class="end">
      {@render row(
        href({ view: "server" }),
        route.view === "server",
        t.nav.server,
        server?.tone ?? null,
        server?.text ?? null,
        null,
      )}
      {@render row(href({ view: "settings" }), route.view === "settings", t.nav.settings, null, null, null)}
    </div>
  {:else}
    <p class="note" role="status">{t.app.loading}</p>
  {/if}
</div>

<style>
  .side-in {
    position: sticky; top: var(--title-h); height: calc(100vh - var(--title-h) - var(--sbar-h)); overflow-y: auto;
    padding: 12px 10px; display: flex; flex-direction: column;
  }
  .grp { font-size: var(--fs-s); font-weight: 600; color: var(--faint); margin: 10px 8px 4px; }
  .grp ~ .grp { margin-top: 18px; }
  .row {
    display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 2px 12px; align-items: center;
    padding: 8px 10px; border-radius: 6px; text-decoration: none;
  }
  .row:hover { background: var(--box); }
  .row[aria-current="page"] { background: var(--sel); }
  .n { font-weight: 500; overflow-wrap: anywhere; }
  .s { grid-column: 1 / -1; color: var(--muted); font-size: var(--fs-s); display: flex; gap: 7px; align-items: center; }
  .s.bad { color: var(--bad); }
  .s.warn { color: var(--warn); }
  .note { color: var(--faint); font-size: var(--fs-s); margin: 6px 10px 0; }
  .note .linkbtn { font-size: var(--fs-s); padding: 0; }
  .end { margin-top: auto; padding-top: 8px; border-top: 1px solid var(--rule-side); }
  .phone-only { display: none; }

  @media (max-width: 760px) {
    .side-in { position: static; height: auto; padding: 8px 8px 40px; }
    .row { padding: 11px 10px; }
    .row[aria-current="page"] { background: none; }
    .row[aria-current="page"]:hover { background: var(--box); }
    .end { margin-top: 18px; }
    .phone-only { display: block; padding: 8px 8px 0; }
  }
</style>
