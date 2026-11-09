<script lang="ts">
  import ConnectionNotice from "./ConnectionNotice.svelte";
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { href, type Route } from "../lib/router.svelte";
  import { machineName, machineTone, machineKind, selfKind, selfTone, serverBackups, sortMachines } from "../lib/derive";
  import { machineRowText, selfRowText, serverRow } from "../lib/text";
  import { f, t } from "../lib/locale";

  let { route }: { route: Route } = $props();

  const status = $derived(app.status);
  const selfSelected = $derived(route.view === "home" || route.view === "self");
  const others = $derived(
    app.machines && status
      ? sortMachines(app.machines.machines, status.machine.backup_id, app.now, status.overdue_after_hours)
      : [],
  );
  const server = $derived(status ? serverRow(status, t, f, app.now) : null);
</script>

<div class="list-inner">
  <div class="product">nestegg</div>
  {#if route.view === "home"}
    <!-- On a phone the list is a screen of its own and needs the notice too. -->
    <div class="phone-only"><ConnectionNotice /></div>
  {/if}
  {#if status}
    <div class="group">
      <h2 class="group-title">{t.nav.thisPc}</h2>
      <ul>
        <li>
          <a class="row" href={href({ view: "self" })} aria-current={selfSelected ? "page" : undefined}>
            <span class="row-name">{status.machine.hostname}</span>
            <span class="row-status"><Dot tone={selfTone(selfKind(status))} />{selfRowText(status, t, f, app.now, serverBackups(app.snapshots))}</span>
          </a>
        </li>
      </ul>
    </div>
    {#if !others.length && app.machinesError}
      <div class="group">
        <h2 class="group-title">{t.nav.others}</h2>
        <p class="group-note">
          {t.load.othersFailed}
          <button class="linkbtn" type="button" onclick={() => app.loadMachines()}>{t.app.retry}</button>
        </p>
      </div>
    {/if}
    {#if others.length}
      <div class="group">
        <h2 class="group-title">{t.nav.others}</h2>
        <ul>
          {#each others as m (m.backup_id)}
            <li>
              <a
                class="row"
                href={href({ view: "pc", id: m.backup_id })}
                aria-current={route.view === "pc" && route.id === m.backup_id ? "page" : undefined}
              >
                <span class="row-name">{machineName(m)}</span>
                <span class="row-status">
                  <Dot tone={machineTone(machineKind(m, app.now, status.overdue_after_hours))} />
                  {machineRowText(m, t, f, app.now, status.overdue_after_hours)}
                </span>
              </a>
            </li>
          {/each}
        </ul>
        <p class="group-note">{t.nav.othersNote}</p>
      </div>
    {/if}
    <div class="list-foot">
      <a class="row" href={href({ view: "server" })} aria-current={route.view === "server" ? "page" : undefined}>
        <span class="row-name">{t.nav.server}</span>
        {#if server}<span class="row-status"><Dot tone={server.tone} />{server.text}</span>{/if}
      </a>
      <a class="foot-link" href={href({ view: "settings" })}>{t.nav.settings}</a>
    </div>
  {:else}
    <p class="group-note" role="status">{t.app.loading}</p>
  {/if}
</div>

<style>
  .list-inner {
    padding: 22px 12px 20px;
    position: sticky; top: 0; height: 100vh; overflow-y: auto;
    display: flex; flex-direction: column;
  }
  .product { font-weight: 600; padding: 0 12px; margin-bottom: 26px; letter-spacing: -0.01em; }
  .group-title { font-size: var(--fs-s); color: var(--faint); padding: 0 12px; margin: 0 0 6px; font-weight: 500; }
  .group-note { font-size: var(--fs-s); color: var(--faint); padding: 0 12px; margin: 4px 0 8px; }
  .group + .group { margin-top: 26px; }

  .row {
    display: grid; grid-template-columns: 1fr; gap: 1px;
    padding: 9px 12px 10px; border-radius: 8px; text-decoration: none;
    position: relative;
  }
  .row:hover { background: var(--hover); }
  .row[aria-current="page"] { background: var(--bg2); }
  .row[aria-current="page"]::before {
    content: ""; position: absolute; left: 0; top: 11px; bottom: 11px; width: 3px;
    border-radius: 2px; background: var(--accent);
  }
  .row-name { font-weight: 500; overflow-wrap: anywhere; }
  .row-status { font-size: var(--fs-s); color: var(--muted); display: flex; align-items: center; gap: 8px; }

  .list-foot { margin-top: auto; padding-top: 28px; }
  .phone-only { display: none; }
  /* The list pane is bg1 already; lift the notice one level. */
  .phone-only :global(.conn) { background: var(--bg2); }
  .group-note .linkbtn { font-size: var(--fs-s); padding: 0; }
  .foot-link { display: block; padding: 9px 12px; border-radius: 8px; text-decoration: none; color: var(--muted); }
  .foot-link:hover { background: var(--hover); color: var(--text); }

  @media (max-width: 760px) {
    .list-inner { position: static; height: auto; min-height: 100vh; padding: 20px 8px 28px; }
    .row { padding: 12px 12px 13px; }
    .row[aria-current="page"] { background: transparent; }
    .row[aria-current="page"]::before { display: none; }
    .list-foot { margin-top: 0; }
    .phone-only { display: block; padding: 0 12px; }
  }
</style>
