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

  /**
   * `remote`: rows of another PC, built from what the PBS reports; there is no
   * log to load and no duration or upload size.
   * `emptyText`: replaces the default text of an empty list.
   */
  let {
    rows,
    loaded,
    remote = false,
    emptyText,
  }: { rows: RunRow[]; loaded: boolean; remote?: boolean; emptyText?: string } = $props();

  const LIMIT = 8;
  let showAll = $state(false);
  /** Rows opened or closed by the user; the newest row starts open when it failed. */
  let toggled = $state<Record<string, boolean>>({});
  const shown = $derived(showAll ? rows : rows.slice(0, LIMIT));
  const cols = $derived(remote ? 4 : 5);

  function isOpen(row: RunRow, i: number): boolean {
    if (!row.expandable) return false;
    return toggled[row.run.id] ?? (i === 0 && row.run.state === "failed");
  }

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
    if (r.state === "failed") return "bad";
    if (r.state === "pbs_unreachable") return "pbs";
    if (r.state === "succeeded" && r.warnings?.length) return "warn";
    return "neutral";
  }

  const slug = (id: string) => `log-${id.replace(/[^A-Za-z0-9_-]/g, "_")}`;

  async function load(id: string) {
    if (remote || logs[id]?.state === "ok" || logs[id]?.state === "loading") return;
    logs[id] = { state: "loading" };
    try {
      logs[id] = { state: "ok", detail: await api.run(id) };
    } catch {
      logs[id] = { state: "error" };
    }
  }

  function toggle(row: RunRow, i: number) {
    const open = !isOpen(row, i);
    toggled[row.run.id] = open;
    if (open) void load(row.run.id);
  }

  // A row that starts open needs its log as well.
  $effect(() => {
    const first = rows[0];
    if (first && isOpen(first, 0)) void load(first.run.id);
  });
</script>

{#if !rows.length}
  {#if loaded}
    <h2 class="sec-title empty-title">{t.runs.title}</h2>
    <p class="empty">{emptyText ?? t.runs.empty}</p>
  {/if}
{:else}
  <table class="tbl runs" class:remote>
    <caption>{t.runs.title}</caption>
    <thead>
      <tr>
        <th scope="col">{t.runs.colDay}</th>
        <th scope="col">{t.runs.colStart}</th>
        <th scope="col">{t.runs.colResult}</th>
        {#if !remote}<th scope="col" class="c-dur">{t.runs.colDuration}</th>{/if}
        <th scope="col" class="r">{#if remote}<span class="sr">{t.runs.showLog}</span>{:else}{t.runs.colUploaded}{/if}</th>
      </tr>
    </thead>
    <tbody>
      {#each shown as row, i (row.run.id)}
        {@const r = row.run}
        {@const open = isOpen(row, i)}
        {@const log = logs[r.id]}
        <tr class="run" class:bad={r.state === "failed"}>
          <td class="m">{f.dayShort(r.started_at)}</td>
          <td class="start">{f.time(r.started_at)}</td>
          <td class="c-res"><span class="res"><Dot tone={tone(r)} /><span class:muted={r.state === "cancelled"}>{what(row)}</span></span></td>
          {#if !remote}<td class="m c-dur">{r.duration_seconds != null ? f.duration(r.duration_seconds) : ""}</td>{/if}
          <td class="r c-end" class:c-empty={!row.expandable && (remote || r.uploaded_bytes == null || r.state !== "succeeded")}>
            {#if row.expandable}
              <button class="linkbtn" type="button" aria-expanded={open} aria-controls={slug(r.id)} onclick={() => toggle(row, i)}>
                {open ? t.runs.hideLog : t.runs.showLog}
              </button>
            {:else if !remote && r.state === "succeeded" && r.uploaded_bytes != null}
              {f.size(r.uploaded_bytes)}
            {/if}
          </td>
        </tr>
        {#if open}
          <tr class="logrow" id={slug(r.id)}>
            <td colspan={cols}><div class="logwrap">
              {#if r.warnings?.length}
                <p class="explain"><strong>{t.runs.warnings}:</strong> {r.warnings.join(" · ")}</p>
              {/if}
              {#if remote}
                <!-- svelte-ignore a11y_no_noninteractive_tabindex (scrollable log must be reachable by keyboard) -->
                {#if r.error?.message}<pre class="log" tabindex="0" aria-label={t.other.reportedError}>{r.error.message}</pre>{/if}
              {:else if !log || log.state === "loading"}
                <p class="explain">{t.runs.logLoading}</p>
              {:else if log.state === "error"}
                <p class="explain">{t.runs.logFailed}</p>
              {:else if log.detail?.log_tail?.length}
                <!-- svelte-ignore a11y_no_noninteractive_tabindex (scrollable log must be reachable by keyboard) -->
                <pre class="log" tabindex="0" aria-label={t.runs.showLog}>{log.detail.log_tail.join("\n")}</pre>
              {:else}
                <p class="explain">{r.error?.message ?? t.runs.logEmpty}</p>
              {/if}
              {#if errorKind(r)}
                <p class="explain">{t.explain[errorKind(r)!]}</p>
              {/if}
            </div></td>
          </tr>
        {/if}
      {/each}
    </tbody>
  </table>
  {#if rows.length > LIMIT}
    <div class="more">
      <button class="btn small" type="button" aria-expanded={showAll} onclick={() => (showAll = !showAll)}>
        {showAll ? t.runs.showFewer : t.runs.showAll(rows.length)}
      </button>
    </div>
  {/if}
{/if}

<style>
  .runs, .empty-title { margin-top: 28px; }
  .start { font-weight: 500; white-space: nowrap; }
  .c-end { white-space: nowrap; }
  tr.bad .c-res { color: var(--bad); }
  .logrow td { padding: 0 0 12px; }
  /* Long log lines scroll inside the log; they must not widen the table. */
  .logwrap { width: 0; min-width: 100%; }
  .log {
    margin: 0; background: var(--side); border: 1px solid var(--rule); border-radius: 6px; padding: 10px 12px;
    font: 12.5px/1.6 var(--mono); color: var(--muted); overflow-x: auto; white-space: pre;
  }
  .explain { margin: 8px 0 0; color: var(--muted); font-size: 13px; max-width: 72ch; }
  .explain:first-child { margin-top: 0; }
  .explain strong { font-weight: 500; color: var(--text); }
  .more { margin-top: 10px; }

  @media (max-width: 760px) {
    thead { display: none; }
    table, tbody, tr, td { display: block; }
    caption { display: block; }
    /* Two lines per run: day, start and size (or the log link); then the result. */
    tr.run {
      display: grid; grid-template-columns: auto auto minmax(0, 1fr); gap: 2px 10px;
      padding: 9px 0; border-bottom: 1px solid var(--rule);
    }
    tr.run td { border: 0; padding: 0; }
    tr.run .m { color: var(--text); }
    tr.run .c-dur { display: none; }
    tr.run .c-end { grid-column: 3; grid-row: 1; text-align: right; color: var(--muted); font-size: 13px; }
    tr.run .c-res { grid-column: 1 / -1; grid-row: 2; }
    .logrow td { border-bottom: 1px solid var(--rule); padding-top: 4px; }
  }
</style>
