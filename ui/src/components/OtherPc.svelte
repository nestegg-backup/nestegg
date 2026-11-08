<script lang="ts">
  import Back from "./Back.svelte";
  import DayStrip from "./DayStrip.svelte";
  import Dot from "./Dot.svelte";
  import KeyNotice from "./KeyNotice.svelte";
  import RunHistory from "./RunHistory.svelte";
  import { app } from "../lib/store.svelte";
  import { dayStrip, historyRows, machineEvents, machineName } from "../lib/derive";
  import { machineSummary } from "../lib/text";
  import { f, t } from "../lib/locale";
  import type { Machine, Run } from "../lib/types";

  let { machine, id }: { machine: Machine | null; id: string } = $props();

  const overdueHours = $derived(app.status?.overdue_after_hours);
  const summary = $derived(machine ? machineSummary(machine, t, f, app.now, overdueHours) : null);
  const cells = $derived(machine ? dayStrip(machineEvents(machine), app.now, f) : []);

  // The PBS only knows snapshots (successes) and the last uploaded report.
  const runs = $derived.by((): Run[] => {
    if (!machine) return [];
    const list: Run[] = (machine.recent_snapshots ?? []).map((time) => ({
      id: `snap-${time}`,
      trigger: "timer",
      state: "succeeded",
      started_at: time,
    }));
    const r = machine.report;
    if (r && r.result !== "succeeded") {
      list.push({
        id: `report-${r.reported_at}`,
        trigger: "timer",
        state: r.result === "failed" ? "failed" : "pbs_unreachable",
        started_at: r.run_started_at ?? r.reported_at,
        error: r.error ?? null,
      });
    }
    return list.sort((a, b) => b.started_at.localeCompare(a.started_at));
  });
  const rows = $derived(historyRows(runs, f));
</script>

<Back />
{#if !machine || !summary}
  <p class="kicker">{t.other.kicker}</p>
  <h1>{id}</h1>
  <p class="lead-text">{app.machines === null ? t.app.loading : t.other.notFound}</p>
{:else}
  {@const name = machineName(machine)}
  {@const report = machine.report}
  <p class="kicker">{t.other.kicker}</p>
  <h1>{name}</h1>
  <p class="lead">{summary.lead}</p>
  <p class="lead-text">{summary.text}</p>

  {#if report?.key_exported === false}
    <KeyNotice {name} />
  {/if}

  <section class="section" aria-labelledby="h-hist">
    <h2 id="h-hist">{t.strip.title}</h2>
    <DayStrip {cells} />
    <RunHistory {rows} loaded remote />
  </section>

  <section class="section" aria-labelledby="h-facts">
    <h2 id="h-facts">{t.other.factsTitle}</h2>
    <dl class="facts">
      <dt>{t.self.factBackupId}</dt>
      <dd><code>{machine.backup_id}</code></dd>
      {#if machine.os}
        <dt>{t.self.factOs}</dt>
        <dd>{machine.os}</dd>
      {/if}
      {#if report?.sources?.length}
        <dt>{t.self.factSources}</dt>
        <dd><span class="paths">{#each report.sources as s (s)}<code>{s}</code>{/each}</span></dd>
      {/if}
      {#if machine.size_bytes != null}
        <dt>{t.self.factOnServer}</dt>
        <dd>{f.size(machine.size_bytes)}</dd>
      {/if}
      <dt>{t.other.factKey}</dt>
      <dd>
        {#if report?.key_exported === true}
          {t.other.yes}
        {:else if report?.key_exported === false}
          <span class="status-word"><Dot tone="warn" />{t.other.no}</span>
        {:else}
          {t.other.unknown}
        {/if}
      </dd>
      {#if report?.nestegg_version}
        <dt>{t.other.factVersion}</dt>
        <dd>{report.nestegg_version}</dd>
      {/if}
      {#if app.machines?.read_at}
        <dt>{t.other.factRead}</dt>
        <dd>{t.other.readAt(f.moment(app.machines.read_at, app.now))}</dd>
      {/if}
    </dl>
  </section>
{/if}
