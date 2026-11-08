<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import LoadError from "./LoadError.svelte";
  import Topline from "./Topline.svelte";
  import { app } from "../lib/store.svelte";
  import { intervalHours } from "../lib/derive";
  import { f, t } from "../lib/locale";
  import type { Status } from "../lib/types";

  let { status }: { status: Status } = $props();

  const c = $derived(app.config.value);
  const configFailed = $derived(app.config.state === "error");
  const p = $derived(status.pbs);
  const hours = $derived(intervalHours(c?.schedule ?? status.schedule.calendar));
  const overdue = $derived(c?.overdue_after_hours ?? status.overdue_after_hours);
  const sources = $derived((c?.sources ?? []).filter((s) => s.archive !== "inventory.pxar"));
  const unconfigured = $derived(status.health === "unconfigured");
</script>

<Topline backHref="#/" backLabel={t.nav.back} />
<h1>{t.settings.title}</h1>

{#if unconfigured}
  <p class="lead-text">{t.settings.unconfigured}</p>
  <Cmd command="nestegg init --help" />
{:else}
  <p class="lead-text">{t.settings.intro}</p>
  <p class="lead-text">{t.settings.checkHint}</p>
  <Cmd command="sudo nestegg check" />
  {#if configFailed}
    <LoadError text={t.load.configFailed} onretry={() => app.loadConfig()} />
  {/if}

  <section class="section" aria-labelledby="s1">
    <h2 id="s1">{t.settings.server}</h2>
    <dl class="facts">
      <dt>{t.server.address}</dt>
      <dd>{p.server}</dd>
      <dt>{t.server.datastore}</dt>
      <dd>{p.datastore}</dd>
      <dt>{t.server.namespace}</dt>
      <dd>{p.namespace || t.server.noNamespace}</dd>
      <dt>{t.settings.backupId}</dt>
      <dd><code>{c?.backup_id ?? status.machine.backup_id}</code></dd>
    </dl>
  </section>

  <section class="section" aria-labelledby="s2">
    <h2 id="s2">{t.settings.wake}</h2>
    <dl class="facts">
      <dt>{t.server.wol}</dt>
      <dd>{p.wake_on_lan ? t.server.on : t.server.off}</dd>
    </dl>
  </section>

  <section class="section" aria-labelledby="s3">
    <h2 id="s3">{t.settings.when}</h2>
    <dl class="facts">
      <dt>{t.settings.interval}</dt>
      <dd>{hours ? t.settings.everyHours(hours) : (c?.schedule ?? status.schedule.calendar ?? "")}</dd>
      <dt>{t.settings.automatic}</dt>
      <dd>{status.schedule.enabled ? t.settings.automaticOn : t.settings.automaticOff}</dd>
      {#if status.schedule.enabled && status.schedule.next_run}
        <dt>{t.settings.nextRun}</dt>
        <dd>{t.at(f.moment(status.schedule.next_run, app.now))}</dd>
      {/if}
      {#if overdue}
        <dt>{t.settings.overdueAfter}</dt>
        <dd>{t.settings.overdueHours(overdue)}</dd>
      {/if}
      <dt>{t.server.retention}</dt>
      <dd>{t.server.retentionText}</dd>
    </dl>
  </section>

  <section class="section" aria-labelledby="s4">
    <h2 id="s4">{t.settings.what}</h2>
    <dl class="facts">
      <dt>{t.settings.folders}</dt>
      <dd>
        {#if c}
          <span class="paths">{#each sources as s (s.archive)}<code>{s.path}</code>{/each}</span>
        {:else}
          <span class="muted">{configFailed ? t.load.notLoaded : t.load.loading}</span>
        {/if}
      </dd>
      <dt>{t.settings.extra}</dt>
      <dd>{t.settings.extraText}</dd>
      <dt>{t.settings.excluded}</dt>
      {#if !c}
        <dd><span class="muted">{configFailed ? t.load.notLoaded : t.load.loading}</span></dd>
      {:else}
        <dd>
          {#if c.excludes?.length}
            <span class="paths">{#each c.excludes as e (e)}<code>{e}</code>{/each}</span>
          {:else}
            {t.settings.nothing}
          {/if}
        </dd>
      {/if}
      <dt>{t.settings.encryption}</dt>
      <dd>
        {!status.encryption.enabled
          ? t.self.encryptionOff
          : status.encryption.key_exported
            ? t.self.encryptionOnKey
            : t.self.encryptionOnNoKey}
      </dd>
      {#if status.encryption.key_fingerprint}
        <dt>{t.settings.fingerprint}</dt>
        <dd><code>{status.encryption.key_fingerprint}</code></dd>
      {/if}
    </dl>
  </section>
{/if}

{#if app.healthInfo}
  <p class="version small faint">{t.settings.version(app.healthInfo.version, app.healthInfo.api_version)}</p>
{/if}

<style>
  .lead-text + .lead-text { margin-top: 14px; }
  .version { margin-top: 52px; }
</style>
