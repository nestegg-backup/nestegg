<script lang="ts">
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { f, t } from "../lib/locale";

  /** Shown while cached data is on screen but the daemon does not answer. */
  const failure = $derived(app.fatal);
</script>

{#if app.offline && failure}
  <div class="conn" role="alert">
    <p class="title"><Dot tone="warn" />{t.conn.lost}</p>
    {#if failure.kind === "unauthorized"}
      <p>{t.conn.authLost}</p>
    {:else}
      {#if app.lastContact}<p>{t.conn.stale(f.moment(app.lastContact.toISOString(), app.now))}</p>{/if}
      {#if failure.message}<p class="small">{t.errors.detail(failure.message)}</p>{/if}
      <button class="linkbtn" type="button" onclick={() => app.retry()}>{t.app.retry}</button>
    {/if}
  </div>
{/if}

<style>
  .conn { background: var(--bg1); border-radius: 8px; padding: 14px 16px; margin-bottom: 32px; max-width: 680px; }
  .title { font-weight: 600; display: flex; align-items: center; gap: 10px; }
  p + p { margin-top: 4px; }
  p:not(.title) { color: var(--muted); }
  .linkbtn { margin-top: 6px; }
</style>
