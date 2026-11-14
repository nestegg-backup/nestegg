<script lang="ts">
  import LoadError from "../components/LoadError.svelte";
  import { hub } from "./store.svelte";
  import { f, t } from "../lib/locale";

  /** "About this hub": household, backup server, master key, package; and logging out. */
  const k = t.hub.facts;
  const h = $derived(hub.hub);
  let leaving = $state(false);

  async function logout() {
    if (leaving) return;
    leaving = true;
    try {
      await hub.logout();
    } catch {
      // The page shows the login either way; the cookie expires on its own.
    } finally {
      leaving = false;
    }
  }
</script>

<h2 class="sec-title" id="h-hub">{k.title}</h2>
{#if h}
  <dl>
    <dt>{k.household}</dt>
    <dd>{h.name}</dd>
    <dt>{k.server}</dt>
    <dd>{h.pbs.server}:{h.pbs.port}</dd>
    <dt>{k.datastore}</dt>
    <dd>{h.pbs.datastore}</dd>
    <dt>{k.namespace}</dt>
    <dd>{h.pbs.namespace || k.noNamespace}</dd>
    <dt>{k.user}</dt>
    <dd><code>{h.pbs.user}</code></dd>
    <dt>{k.wol}</dt>
    <dd>{h.pbs.wake_on_lan ? k.on : k.off}</dd>
    <dt>{k.masterKey}</dt>
    <dd>
      {#if !h.master_key.present}
        <span class="warn-text">{k.masterMissing}</span>
      {:else if h.master_key.proven}
        {h.master_key.proven_at ? k.masterProven(f.moment(h.master_key.proven_at, hub.now)) : k.masterProvenUndated}
      {:else}
        <span class="warn-text">{k.masterUnproven}</span>
      {/if}
    </dd>
    {#if h.master_key.fingerprint}
      <dt>{k.fingerprint}</dt>
      <dd><code class="fp">{h.master_key.fingerprint}</code></dd>
    {/if}
    <dt>{k.package}</dt>
    <dd>
      {#if h.package.available && h.package.version}
        {k.packageVersion(h.package.version, h.package.architecture ?? null)}
      {:else if h.package.available}
        {t.other.yes}
      {:else}
        <span class="warn-text">{k.packageNone}</span>
      {/if}
    </dd>
    <dt>{k.address}</dt>
    <dd>{h.url}</dd>
    <dt>{k.version}</dt>
    <dd>{h.version}</dd>
  </dl>
{:else if hub.hubError && !hub.hubError.unauthorized}
  <LoadError text={k.loadFailed} onretry={() => hub.loadHub()} />
{:else}
  <p class="muted" role="status">{t.hub.loading}</p>
{/if}
<div class="out">
  <button class="btn" type="button" aria-disabled={leaving ? "true" : undefined} onclick={logout}>
    {leaving ? t.hub.loggingOut : t.hub.logout}
  </button>
</div>

<style>
  .warn-text { color: var(--warn); }
  .fp { font-size: 12px; overflow-wrap: anywhere; }
  .out { margin-top: 22px; }
</style>
