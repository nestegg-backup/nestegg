<script lang="ts">
  import Dot from "../components/Dot.svelte";
  import LoadError from "../components/LoadError.svelte";
  import AddComputer from "./AddComputer.svelte";
  import HubFacts from "./HubFacts.svelte";
  import MachineItem from "./MachineItem.svelte";
  import OpenCodes from "./OpenCodes.svelte";
  import { overview, sortHubMachines } from "./derive";
  import { hub } from "./store.svelte";
  import { f, t } from "../lib/locale";

  const data = $derived(hub.machines);
  const ov = $derived(data ? overview(data, t, hub.now) : null);
  const sorted = $derived(data ? sortHubMachines(data.machines, data, hub.now) : []);
  let adding = $state(false);
  let listHeading: HTMLElement | undefined = $state();
</script>

<div class="head">
  <div class="status">
    {#if ov}
      <h2 class="lead" class:bad={ov.tone === "bad"}>
        {#if ov.tone === "bad" || ov.tone === "warn"}<Dot tone={ov.tone} />{/if}{ov.lead}
      </h2>
      <p class="lead-text">{ov.text}</p>
    {:else if hub.machinesError}
      <LoadError text={t.hub.load.machinesFailed} onretry={() => hub.refresh()} />
    {:else}
      <p class="lead-text" role="status">{t.hub.loading}</p>
    {/if}
  </div>
  <div class="acts">
    <button class="btn primary" type="button" onclick={() => (adding = true)}>{t.hub.dialog.title}</button>
  </div>
</div>

{#if hub.offline}
  <div class="notice-box note" role="alert">
    <p class="title"><Dot tone="warn" />{t.hub.offline}</p>
  </div>
{/if}

{#if data && !data.pbs_reachable}
  <div class="notice-box note" role="status">
    {#if data.pbs_read_at}
      <p class="title"><Dot tone="pbs" />{t.hub.pbs.asleep(f.moment(data.pbs_read_at, hub.now))}</p>
      <p>{t.hub.pbs.asleepText}</p>
    {:else}
      <p class="title"><Dot tone="pbs" />{t.hub.pbs.never}</p>
      <p>{t.hub.pbs.neverText}</p>
    {/if}
  </div>
{/if}

<div class="cols">
  <div class="list-col">
    <section aria-labelledby="h-machines">
      <h2 class="sec-title" id="h-machines" tabindex="-1" bind:this={listHeading}>{t.hub.list.title}</h2>
      {#if data && sorted.length}
        <ul class="machines">
          {#each sorted as m (m.machine_id)}
            <MachineItem machine={m} {data} onremoved={() => listHeading?.focus()} />
          {/each}
        </ul>
      {:else if data}
        <p class="empty">{t.hub.list.empty}</p>
      {:else if hub.machinesError}
        <LoadError text={t.hub.load.machinesFailed} onretry={() => hub.refresh()} />
      {:else}
        <p class="empty">{t.hub.loading}</p>
      {/if}
    </section>

    <OpenCodes />

    {#if data?.unmanaged.length}
      <section class="block unmanaged" aria-labelledby="h-unmanaged">
        <h2 class="sec-title" id="h-unmanaged">{t.hub.unmanaged.title}</h2>
        <p class="hint intro">{t.hub.unmanaged.text}</p>
        <ul>
          {#each data.unmanaged as g (g.backup_id)}
            <li>
              <code>{g.backup_id}</code>
              <span class="muted">{g.last_snapshot ? t.hub.unmanaged.last(f.moment(g.last_snapshot, hub.now)) : t.hub.unmanaged.none}</span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}
  </div>

  <aside class="about" aria-labelledby="h-hub">
    <HubFacts />
  </aside>
</div>

<AddComputer bind:open={adding} />

<style>
  .note { margin-top: 20px; max-width: 720px; }
  .note .title { font-weight: 600; display: flex; align-items: center; gap: 10px; }
  .note p + p { margin-top: 4px; color: var(--muted); }
  .list-col { min-width: 0; }
  .machines { border-top: 1px solid var(--rule); }
  .block { margin-top: 32px; }
  .intro { margin: -4px 0 10px; max-width: 66ch; }
  .unmanaged ul { border-top: 1px solid var(--rule); }
  .unmanaged li {
    display: flex; flex-wrap: wrap; gap: 2px 14px; align-items: baseline; justify-content: space-between;
    padding: 9px 0; border-bottom: 1px solid var(--rule);
  }
  .unmanaged code { overflow-wrap: anywhere; }
  .unmanaged .muted { font-size: var(--fs-s); }
  h2[tabindex]:focus { outline: none; }
</style>
