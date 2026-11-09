<script lang="ts">
  import Dot from "./Dot.svelte";
  import LoadError from "./LoadError.svelte";
  import { app } from "../lib/store.svelte";
  import { tick, untrack } from "svelte";
  import { historyRows, serverContact } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Status } from "../lib/types";

  let { status }: { status: Status } = $props();

  const p = $derived(status.pbs);
  const check = $derived(app.pbsCheck);
  const info = $derived(app.pbs);
  const bad = $derived(check.state === "done" ? check.result.checks.filter((c) => !c.ok).length : 0);
  // The daemon names this item; a server that does not answer is a failed item, not an error.
  const reachFailed = $derived(check.state === "done" && check.result.checks.some((c) => c.name === "PBS reachable" && !c.ok));

  let button: HTMLButtonElement | undefined = $state();
  let resultEl: HTMLElement | undefined = $state();
  let errorEl: HTMLElement | undefined = $state();
  /** Said once through the live region when the focus was elsewhere at the end of the check. */
  let announced = $state("");

  async function run() {
    if (app.pbsCheck.state === "running" || app.offline) return;
    announced = "";
    await app.checkPbs();
    await tick();
    const a = document.activeElement;
    // The button is "busy" and keeps the focus. At the end the focus goes to the result, unless
    // the user went elsewhere meanwhile: then the live region says it.
    const here = !a || a === document.body || a === button;
    const end = app.pbsCheck;
    if (here && end.state === "done") resultEl?.focus();
    else if (here && end.state === "error") errorEl?.focus();
    else if (end.state === "done") announced = t.server.checkFinished(end.result.checks.filter((c) => !c.ok).length);
    else if (end.state === "error") announced = t.server.checkFailed(end.message);
  }

  $effect(() => {
    // Disk usage of the datastore, when the daemon can tell.
    untrack(() => void app.loadPbs());
  });
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
    <!-- The one live region of the check: it announces that the check runs, and the result when the
         focus is somewhere else. Otherwise the focus moves to the result and is read from there. -->
    <p class="hint live" role="status">{check.state === "running" ? (check.wake ? t.server.checkingWake : t.server.checkingText) : announced}</p>
    {#if check.state === "error"}
      <p class="hint bad" tabindex="-1" bind:this={errorEl}>{t.server.checkFailed(check.message)}</p>
    {/if}
  </div>
  <div class="acts">
    <!-- aria-disabled instead of disabled: the button keeps the focus while the check runs. -->
    <button
      class="btn"
      type="button"
      bind:this={button}
      disabled={app.offline}
      aria-disabled={check.state === "running" ? "true" : undefined}
      onclick={run}
    >
      {check.state === "running" ? t.server.checking : t.server.check}
    </button>
  </div>
</div>

{#if check.state === "done"}
  <section class="result notice-box" aria-labelledby="h-check" aria-describedby="check-sum" tabindex="-1" bind:this={resultEl}>
    <h2 id="h-check">{t.server.checkTitle}</h2>
    <p class="hint" id="check-sum">
      {t.server.checkedAt(f.moment(check.result.checked_at, app.now))}
      {bad ? t.server.checkProblems(bad) : t.server.checkAllOk}
    </p>
    {#if reachFailed}
      <p class="hint bad">{t.server.checkPbs}{check.wake ? ` ${t.server.checkPbsWol}` : ""}</p>
    {/if}
    <ul>
      {#each check.result.checks as c}
        <li>
          <span class="res">
            <Dot tone={c.ok ? "neutral" : "bad"} />
            <span>
              <strong>{c.name}</strong>
              <span class="sr">: {c.ok ? t.server.checkOkLabel : t.server.checkBadLabel}</span>
              {#if c.detail}<span class="detail" class:bad={!c.ok}>{c.detail}</span>{/if}
            </span>
          </span>
        </li>
      {/each}
    </ul>
    <p class="hint">{t.server.checkNames}</p>
  </section>
{/if}

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
      {#if info && info.used_bytes != null}
        <dt>{t.server.usage}</dt>
        <dd>{info.total_bytes != null ? t.server.usageOf(f.size(info.used_bytes), f.size(info.total_bytes)) : f.size(info.used_bytes)}</dd>
      {/if}
      <dt>{t.server.wol}</dt>
      <dd>{p.wake_on_lan ? t.server.on : t.server.off}</dd>
      <dt>{t.server.retention}</dt>
      <dd>{t.server.retentionText}</dd>
    </dl>
  </aside>
</div>

<style>
  .live:empty { position: absolute; margin: 0; }
  .result { margin-top: 22px; max-width: 720px; padding: 14px 16px 16px; }
  .result h2 { font-size: var(--fs-m); font-weight: 600; }
  .result ul { margin-top: 8px; display: grid; gap: 6px; }
  .result .detail { display: block; color: var(--muted); font-size: 13px; overflow-wrap: anywhere; }
  .result .detail.bad { color: var(--bad); }
  .contacts { margin-top: 10px; }
  .start { font-weight: 500; white-space: nowrap; }
  @media (max-width: 760px) {
    .contacts td:first-child { white-space: normal; }
  }
</style>
