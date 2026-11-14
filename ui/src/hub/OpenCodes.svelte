<script lang="ts">
  import { asHubFailure } from "./api";
  import { minutesLeft } from "./derive";
  import { hub } from "./store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";

  /** One-time codes that are still valid: what they were for, until when, and revoking them. */
  const c = t.hub.codes;
  // A code that expired since the last answer is no longer offered.
  const codes = $derived((hub.codes ?? []).filter((x) => minutesLeft(x.expires_at, hub.now) > 0));
  let busy = $state<string | null>(null);
  let problem = $state<string | null>(null);
  let heading: HTMLElement | undefined = $state();

  async function revoke(id: string) {
    if (busy) return;
    busy = id;
    problem = null;
    try {
      await hub.revoke(id);
      statusBar.show(c.revoked);
      heading?.focus();
    } catch (e) {
      const fail = asHubFailure(e);
      if (!fail.unauthorized) problem = c.revokeFailed(fail.message);
    } finally {
      busy = null;
    }
  }
</script>

{#if codes.length || problem}
  <section class="block codes" aria-labelledby="h-codes">
    <h2 class="sec-title" id="h-codes" tabindex="-1" bind:this={heading}>{c.title}</h2>
    <p class="hint intro">{c.text}</p>
    <ul>
      {#each codes as code (code.code_id)}
        {@const label = code.note || c.noNote}
        <li>
          <span class="what">
            <span class:muted={!code.note}>{label}</span>
            <span class="until">{c.until(f.moment(code.expires_at, hub.now))}</span>
          </span>
          <button
            class="btn small"
            type="button"
            aria-label={c.revokeLabel(label)}
            aria-disabled={busy ? "true" : undefined}
            onclick={() => revoke(code.code_id)}>{c.revoke}</button
          >
        </li>
      {/each}
    </ul>
    {#if problem}<p class="hint bad" role="alert">{problem}</p>{/if}
  </section>
{/if}

<style>
  .block { margin-top: 32px; }
  .intro { margin: -4px 0 10px; max-width: 66ch; }
  ul { border-top: 1px solid var(--rule); }
  li {
    display: flex; gap: 8px 16px; align-items: center; justify-content: space-between; flex-wrap: wrap;
    padding: 9px 0; border-bottom: 1px solid var(--rule);
  }
  .what { display: flex; flex-direction: column; min-width: 0; overflow-wrap: anywhere; }
  .until { color: var(--muted); font-size: var(--fs-s); }
  h2[tabindex]:focus { outline: none; }
</style>
