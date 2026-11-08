<script lang="ts">
  import Back from "./Back.svelte";
  import Dot from "./Dot.svelte";
  import LoadError from "./LoadError.svelte";
  import { app } from "../lib/store.svelte";
  import { historyRows, serverContact } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Status } from "../lib/types";

  let { status }: { status: Status } = $props();

  const p = $derived(status.pbs);
  const lead = $derived(
    p.reachable === true ? t.server.reachableLead : p.reachable === false ? t.server.unreachableLead : t.server.unknownLead,
  );
  const text = $derived(
    p.reachable == null || !p.checked_at ? t.server.unknownText : t.server.checked(f.moment(p.checked_at, app.now)),
  );
  // Finished runs and what they prove about the server (see serverContact).
  const contacts = $derived(
    historyRows(app.runs.value.filter((r) => r.state === "succeeded" || r.state === "failed" || r.state === "pbs_unreachable"), f)
      .filter((row) => !(row.run.state === "failed" && row.run.error?.kind === "config"))
      .slice(0, 8),
  );
</script>

<Back />
<p class="kicker">{p.server}</p>
<h1>{t.server.title}</h1>
<p class="lead">{lead}</p>
<p class="lead-text">{text}</p>

<section class="section" aria-labelledby="h-contacts">
  <h2 id="h-contacts">{t.server.contactsTitle}</h2>
  {#if app.runs.state === "error"}
    <LoadError text={t.load.runsFailed} onretry={() => app.loadRuns()} />
  {:else if app.runs.state === "loading"}
    <p class="empty" role="status">{t.app.loading}</p>
  {:else if contacts.length}
    <p class="muted">{t.server.contactsText}</p>
    <ul class="runs">
      {#each contacts as row (row.run.id)}
        {@const c = serverContact(row.run)}
        <li class="contact">
          <span class="when">{f.dayShort(row.run.started_at)}</span>
          <span class="time">{f.time(row.run.started_at)}</span>
          <span class="what">
            <Dot tone={c === "not_reached" ? "pbs" : "neutral"} />
            <span class:muted={c === "unknown"}>
              {c === "reached"
                ? t.server.contactOk
                : c === "unknown"
                  ? t.server.contactUnknown
                  : row.count > 1
                    ? t.server.contactPbsTimes(row.count)
                    : t.server.contactPbs}
            </span>
          </span>
        </li>
      {/each}
    </ul>
  {:else}
    <p class="empty">{t.server.contactsEmpty}</p>
  {/if}
</section>

<section class="section" aria-labelledby="h-where">
  <h2 id="h-where">{t.server.storageTitle}</h2>
  <dl class="facts">
    <dt>{t.server.address}</dt>
    <dd>{p.server}</dd>
    <dt>{t.server.datastore}</dt>
    <dd>{p.datastore}</dd>
    <dt>{t.server.namespace}</dt>
    <dd>{p.namespace || t.server.noNamespace}</dd>
    <dt>{t.server.wol}</dt>
    <dd>{p.wake_on_lan ? t.server.on : t.server.off}</dd>
    <dt>{t.server.retention}</dt>
    <dd>{t.server.retentionText}</dd>
  </dl>
</section>

<style>
  .contact { display: grid; grid-template-columns: 108px var(--time-col) minmax(0, 1fr); gap: 4px 16px; align-items: baseline; padding: 11px 0; }
  .when, .time { color: var(--muted); }
  .what { display: flex; align-items: center; gap: 8px; min-width: 0; }
  @media (max-width: 760px) {
    .contact { grid-template-columns: minmax(0, 1fr) auto; }
    .time { text-align: right; }
    .what { grid-column: 1 / -1; }
  }
</style>
