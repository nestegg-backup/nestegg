<script lang="ts">
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
      .filter(
        (row) =>
          !(
            row.run.state === "failed" &&
            (row.run.error?.kind === "config" || row.run.error?.kind === "credentials_unreadable")
          ),
      )
      .slice(0, 8),
  );
</script>

<div class="head">
  <div class="status">
    <h2 class="lead">{#if p.reachable === false}<Dot tone="pbs" />{/if}{lead}</h2>
    <p class="lead-text">{text}</p>
  </div>
</div>

<div class="cols">
  <section aria-labelledby="h-contacts">
    <h2 class="sec-title" id="h-contacts">{t.server.contactsTitle}</h2>
    {#if app.runs.state === "error"}
      <LoadError text={t.load.runsFailed} onretry={() => app.loadRuns()} />
    {:else if app.runs.state === "loading"}
      <p class="empty" role="status">{t.app.loading}</p>
    {:else if contacts.length}
      <p class="lead-text small">{t.server.contactsText}</p>
      <table class="tbl contacts">
        <thead>
          <tr>
            <th scope="col">{t.runs.colDay}</th>
            <th scope="col">{t.runs.colStart}</th>
            <th scope="col">{t.runs.colResult}</th>
          </tr>
        </thead>
        <tbody>
          {#each contacts as row (row.run.id)}
            {@const c = serverContact(row.run)}
            <tr>
              <td class="m">{f.dayShort(row.run.started_at)}</td>
              <td class="start">{f.time(row.run.started_at)}</td>
              <td>
                <span class="res">
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
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <p class="empty">{t.server.contactsEmpty}</p>
    {/if}
  </section>

  <aside class="about" aria-labelledby="h-where">
    <h2 class="sec-title" id="h-where">{t.server.storageTitle}</h2>
    <dl>
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
  </aside>
</div>

<style>
  .contacts { margin-top: 10px; }
  .start { font-weight: 500; white-space: nowrap; }
  @media (max-width: 760px) {
    .contacts td:first-child { white-space: normal; }
  }
</style>
