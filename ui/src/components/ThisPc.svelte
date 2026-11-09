<script lang="ts">
  import { untrack } from "svelte";
  import Cmd from "./Cmd.svelte";
  import DayStrip from "./DayStrip.svelte";
  import Dot from "./Dot.svelte";
  import KeyNotice from "./KeyNotice.svelte";
  import LoadError from "./LoadError.svelte";
  import RunHistory from "./RunHistory.svelte";
  import RunProgress from "./RunProgress.svelte";
  import { app } from "../lib/store.svelte";
  import { href } from "../lib/router.svelte";
  import { historyRows, restoreOffer, selfStrip, selfTone, serverBackups } from "../lib/derive";
  import { selfSummary } from "../lib/text";
  import { f, t } from "../lib/locale";
  import type { Status } from "../lib/types";

  let { status }: { status: Status } = $props();

  const server = $derived(serverBackups(app.snapshots));
  const summary = $derived(selfSummary(status, t, f, app.now, server));
  const tone = $derived(selfTone(summary.kind));
  const cells = $derived(selfStrip(app.runs.value, status.current_run, app.snapshots.list, app.now, f));
  const rows = $derived(historyRows(app.runs.value, f));
  const running = $derived(summary.kind === "running" && !!status.current_run);
  const canRun = $derived(summary.kind !== "unconfigured" && summary.kind !== "key_missing" && summary.kind !== "running");
  // Based on the snapshots on the server, not on the local history (empty after a reinstall).
  const offer = $derived(restoreOffer(summary.kind, app.snapshots));
  const canRestore = $derived(offer === "show");
  const sources = $derived((app.config.value?.sources ?? []).filter((s) => s.archive !== "inventory.pxar"));
  const latest = $derived(app.snapshots.state === "ok" ? (app.snapshots.list[0] ?? null) : null);
  const next = $derived(status.schedule.next_run ?? null);

  $effect(() => {
    // What the server holds decides about restoring, also without any local history.
    // Needs the PBS, so only asked once per page load.
    if (status.health !== "unconfigured") untrack(() => void app.loadSnapshots());
  });
</script>

<div class="head" class:solo={summary.kind === "unconfigured"}>
  <div class="status" aria-live="polite">
    {#if running && status.current_run}
      <RunProgress run={status.current_run} wakeOnLan={!!status.pbs.wake_on_lan} stale={app.offline} />
    {:else}
      <h2 class="lead" class:bad={tone === "bad"}>{#if tone === "bad" || tone === "warn"}<Dot {tone} />{/if}{summary.lead}</h2>
      <p class="lead-text">{summary.text}</p>
      {#if summary.kind === "unconfigured"}
        <Cmd command="nestegg init --help" />
      {/if}
      {#if offer === "empty" && status.last_success}
        <!-- The local history knows a backup, the server has none: say so instead of a dead end. -->
        <p class="hint" role="status">{t.restore.noSnapshots}</p>
      {/if}
    {/if}
    {#if app.startError}
      <p class="hint" class:bad={app.startError.kind !== "conflict"} role="status">
        {app.startError.kind === "conflict" ? t.self.runConflict : t.self.runStartFailed(app.startError.message)}
      </p>
    {/if}
  </div>
  {#if canRun || canRestore}
    <div class="acts">
      {#if canRestore}
        <!-- Restoring needs the daemon; without a connection the link is disabled. -->
        <a
          class="btn"
          href={app.offline ? undefined : href({ view: "restore", step: "1" })}
          aria-disabled={app.offline ? "true" : undefined}
          role={app.offline ? "link" : undefined}>{t.self.restore}</a
        >
      {/if}
      {#if canRun}
        <button class="btn primary" type="button" disabled={app.starting || app.offline} onclick={() => app.startRun()}>
          {app.starting ? t.self.starting : t.self.runNow}
        </button>
      {/if}
    </div>
  {/if}
</div>

{#if summary.kind === "key_missing"}
  <KeyNotice />
{/if}

{#if summary.kind !== "unconfigured"}
  <div class="cols">
    <section aria-label={t.strip.title}>
      {#if app.runs.state === "error"}
        <!-- Never draw 14 empty days for a history that could not be loaded. -->
        <h2 class="sec-title">{t.strip.title}</h2>
        <LoadError text={t.load.runsFailed} onretry={() => app.loadRuns()} />
      {:else if app.runs.state === "loading"}
        <h2 class="sec-title">{t.strip.title}</h2>
        <p class="empty" role="status">{t.app.loading}</p>
      {:else}
        <DayStrip {cells} />
        <RunHistory {rows} loaded emptyText={server?.count ? t.runs.emptyHere : undefined} />
      {/if}
    </section>

    <aside class="about" aria-labelledby="h-about">
      <h2 class="sec-title" id="h-about">{t.self.factsTitle}</h2>
      <dl>
        {#if status.machine.os}
          <dt>{t.self.factOs}</dt>
          <dd>{status.machine.os}</dd>
        {/if}
        <dt>{t.self.factBackupId}</dt>
        <dd><code>{status.machine.backup_id}</code></dd>
        <dt>{t.self.factSources}</dt>
        <dd>
          {#if app.config.state === "error"}
            <span class="muted">{t.load.notLoaded}</span>
            <button class="linkbtn small" type="button" onclick={() => app.loadConfig()}>{t.app.retry}</button>
          {:else if app.config.state === "loading"}
            <span class="muted">{t.load.loading}</span>
          {:else}
            {sources.map((s) => s.path).join(", ")} {t.self.factSourcesExtra}
          {/if}
        </dd>
        {#if app.snapshots.state !== "idle"}
          <dt>{t.self.factOnServer}</dt>
          <dd>
            {#if latest}
              {latest.size_bytes != null
                ? t.self.onServer(f.size(latest.size_bytes), app.snapshots.list.length)
                : t.self.onServerCount(app.snapshots.list.length)}
            {:else if app.snapshots.state === "ok"}
              {t.self.onServerCount(0)}
            {:else if app.snapshots.state === "loading"}
              <span class="muted">{t.load.loading}</span>
            {:else}
              <span class="muted">
                {app.snapshots.error?.kind === "pbs_unreachable" ? t.load.snapshotsPbs : t.load.snapshotsFailed}
              </span>
              <button class="linkbtn small" type="button" onclick={() => app.loadSnapshots(true)}>{t.app.retry}</button>
            {/if}
          </dd>
        {/if}
        <dt>{t.self.factNext}</dt>
        <dd>
          {#if !status.schedule.enabled}
            {t.self.nextOff}
          {:else if running}
            {t.self.runningLead}
          {:else if next}
            {new Date(next) <= app.now ? t.self.nextDue : t.self.nextAt(f.moment(next, app.now))}
          {:else}
            <span class="muted">{t.other.unknown}</span>
          {/if}
        </dd>
        <dt>{t.self.factEncryption}</dt>
        <dd>
          {#if !status.encryption.enabled}
            {t.self.encryptionOff}
          {:else if status.encryption.key_exported}
            {t.self.encryptionOnKey}
          {:else}
            <span class="warn-text">{t.self.encryptionOnNoKey}</span>
          {/if}
        </dd>
      </dl>
    </aside>
  </div>
{/if}

<style>
  .warn-text { color: var(--warn); }
</style>
