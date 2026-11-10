<script lang="ts">
  import DayStrip from "./DayStrip.svelte";
  import Dot from "./Dot.svelte";
  import KeyNotice from "./KeyNotice.svelte";
  import RunHistory from "./RunHistory.svelte";
  import { app } from "../lib/store.svelte";
  import { dayStrip, historyRows, machineName, machineSnapshotEvents, machineTone } from "../lib/derive";
  import { machineSummary } from "../lib/text";
  import { f, t } from "../lib/locale";
  import type { Machine, Run } from "../lib/types";

  let { machine, id }: { machine: Machine | null; id: string } = $props();

  const overdueHours = $derived(app.status?.overdue_after_hours);
  const summary = $derived(machine ? machineSummary(machine, t, f, app.now, overdueHours) : null);
  const tone = $derived(summary ? machineTone(summary.kind) : "neutral");
  // Snapshot days only, like its mini strip: the PBS keeps no trace of failed runs.
  const cells = $derived(machine ? dayStrip(machineSnapshotEvents(machine), app.now, f) : []);

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
        // Any result this version does not know is shown as "not understood", never as a known one.
        state: r.result === "failed" ? "failed" : r.result === "pbs_unreachable" ? "pbs_unreachable" : "unknown",
        started_at: r.run_started_at ?? r.reported_at,
        error: r.error ?? null,
      });
    }
    return list.sort((a, b) => b.started_at.localeCompare(a.started_at));
  });
  const rows = $derived(historyRows(runs, f));
</script>

{#if !machine || !summary}
  <div class="head">
    <div class="status">
      <p class="lead-text" role="status">{app.machines === null ? t.app.loading : t.other.notFound}</p>
    </div>
  </div>
{:else}
  {@const name = machineName(machine)}
  {@const report = machine.report}
  <div class="head">
    <div class="status">
      <h2 class="lead" class:bad={tone === "bad"}>{#if tone === "bad" || tone === "warn"}<Dot {tone} />{/if}{summary.lead}</h2>
      <p class="lead-text">{summary.text}</p>
    </div>
  </div>

  {#if report?.key_exported === false}
    <KeyNotice {name} />
  {/if}

  <div class="cols">
    <section aria-label={t.strip.title}>
      <DayStrip {cells} />
      <RunHistory {rows} loaded remote />
    </section>

    <aside class="about" aria-labelledby="h-about">
      <h2 class="sec-title" id="h-about">{t.other.factsTitle}</h2>
      <dl>
        {#if machine.os}
          <dt>{t.self.factOs}</dt>
          <dd>{machine.os}</dd>
        {/if}
        <dt>{t.self.factBackupId}</dt>
        <dd><code>{machine.backup_id}</code></dd>
        {#if report?.sources?.length}
          <dt>{t.self.factSources}</dt>
          <dd>{report.sources.join(", ")}</dd>
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
            <span class="warn-text">{t.other.no}</span>
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
      <p class="hint">{t.nav.othersNote}</p>
    </aside>
  </div>
{/if}

<style>
  .warn-text { color: var(--warn); }
</style>
