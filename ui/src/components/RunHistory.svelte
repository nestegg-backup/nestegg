<script lang="ts" module>
  import type { RunDetail } from "../lib/types";

  // Log tails, loaded when a row is opened. Finished runs do not change.
  const logs = $state<Record<string, { state: "loading" | "ok" | "error"; detail?: RunDetail }>>({});
</script>

<script lang="ts">
  import Dot from "./Dot.svelte";
  import { api } from "../lib/api";
  import { errorKind, type RunRow } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Run } from "../lib/types";

  /** `remote`: rows of another PC, built from what the PBS reports; there is no log to load. */
  let { rows, loaded, remote = false }: { rows: RunRow[]; loaded: boolean; remote?: boolean } = $props();
  let showAll = $state(false);
  const shown = $derived(showAll ? rows : rows.slice(0, 7));

  function what(row: RunRow): string {
    const r = row.run;
    switch (r.state) {
      case "succeeded":
        if (r.warnings?.length) return t.runs.succeededWarnings(r.warnings.length);
        return r.trigger === "timer" ? t.runs.succeeded : t.runs.succeededManual;
      case "failed":
        return t.runs.failed(t.reason[errorKind(r) ?? "internal"]);
      case "pbs_unreachable":
        return row.count > 1 ? t.runs.pbsTimes(row.count) : t.runs.pbs;
      default:
        return t.runs.cancelled;
    }
  }

  function tone(r: Run) {
    return r.state === "failed" ? "bad" : r.state === "pbs_unreachable" ? "pbs" : "neutral";
  }

  function extra(r: Run): string {
    if (r.state !== "succeeded") return "";
    const parts: string[] = [];
    if (r.duration_seconds != null) parts.push(f.duration(r.duration_seconds));
    if (r.uploaded_bytes != null) parts.push(t.runs.uploaded(f.size(r.uploaded_bytes)));
    return parts.join(", ");
  }

  async function load(id: string) {
    if (logs[id]?.state === "ok" || logs[id]?.state === "loading") return;
    logs[id] = { state: "loading" };
    try {
      logs[id] = { state: "ok", detail: await api.run(id) };
    } catch {
      logs[id] = { state: "error" };
    }
  }
</script>

{#if !rows.length}
  {#if loaded}<p class="empty">{t.runs.empty}</p>{/if}
{:else}
  <ul class="runs">
    {#each shown as row (row.run.id)}
      {@const r = row.run}
      {@const log = logs[r.id]}
      {#if row.expandable}
        <li>
          <details class="run" ontoggle={(e) => !remote && (e.currentTarget as HTMLDetailsElement).open && load(r.id)}>
            <summary>
              <span class="when">{f.dayShort(r.started_at)}</span>
              <span class="time">{f.time(r.started_at)}</span>
              <span class="what"><Dot tone={tone(r)} /><span>{what(row)}</span></span>
              <span class="extra"><span class="more more-closed">{t.runs.showLog}</span><span class="more more-open">{t.runs.hideLog}</span></span>
            </summary>
            {#if r.warnings?.length}
              <p class="log-explain"><strong>{t.runs.warnings}:</strong> {r.warnings.join(" · ")}</p>
            {/if}
            {#if remote}
              <!-- svelte-ignore a11y_no_noninteractive_tabindex (scrollable log must be reachable by keyboard) -->
              {#if r.error?.message}<div class="log" tabindex="0" role="region" aria-label={t.other.reportedError}>{r.error.message}</div>{/if}
            {:else if !log || log.state === "loading"}
              <p class="log-explain">{t.runs.logLoading}</p>
            {:else if log.state === "error"}
              <p class="log-explain">{t.runs.logFailed}</p>
            {:else if log.detail?.log_tail?.length}
              <!-- svelte-ignore a11y_no_noninteractive_tabindex (scrollable log must be reachable by keyboard) -->
              <div class="log" tabindex="0" role="region" aria-label={t.runs.showLog}>{log.detail.log_tail.join("\n")}</div>
            {:else}
              <p class="log-explain">{r.error?.message ?? t.runs.logEmpty}</p>
            {/if}
            {#if errorKind(r)}
              <p class="log-explain">{t.explain[errorKind(r)!]}</p>
            {/if}
          </details>
        </li>
      {:else}
        <li class="run">
          <span class="when">{f.dayShort(r.started_at)}</span>
          <span class="time">{f.time(r.started_at)}</span>
          <span class="what"><Dot tone={tone(r)} /><span class:muted={r.state === "cancelled"}>{what(row)}</span></span>
          <span class="extra">{extra(r)}</span>
        </li>
      {/if}
    {/each}
    {#if rows.length > 7}
      <li class="more-row">
        <button class="linkbtn" type="button" aria-expanded={showAll} onclick={() => (showAll = !showAll)}>
          {showAll ? t.runs.showFewer : t.runs.showAll(rows.length)}
        </button>
      </li>
    {/if}
  </ul>
{/if}

<style>
  .run, .run > summary {
    display: grid; grid-template-columns: 108px var(--time-col) minmax(0, 1fr) auto; gap: 4px 16px;
    align-items: baseline;
  }
  .run { padding: 11px 0; }
  details.run { display: block; }
  details.run > summary { cursor: pointer; list-style: none; }
  details.run > summary::-webkit-details-marker { display: none; }
  .when, .time { color: var(--muted); }
  .what { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .extra { color: var(--faint); font-size: var(--fs-s); text-align: right; }
  .more { color: var(--accent); font-size: var(--fs-s); text-decoration: underline; text-underline-offset: 3px; }
  details.run[open] .more-closed, details.run:not([open]) .more-open { display: none; }
  .more-row { padding: 14px 0 0; }
  .log {
    margin: 12px 0 4px; background: var(--bg1); border-radius: 8px; padding: 14px 16px;
    font-family: var(--mono); font-size: var(--fs-s); line-height: 1.7; color: var(--muted);
    overflow-x: auto; white-space: pre;
  }
  .log-explain { margin-top: 10px; color: var(--muted); max-width: 60ch; }
  .log-explain strong { font-weight: 500; color: var(--text); }
  @media (max-width: 760px) {
    .run, .run > summary { grid-template-columns: minmax(0, 1fr) auto; }
    .when { grid-column: 1; }
    .time { grid-column: 2; text-align: right; }
    .what { grid-column: 1 / -1; }
    .extra { grid-column: 1 / -1; text-align: left; }
  }
</style>
