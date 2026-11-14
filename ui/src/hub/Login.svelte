<script lang="ts">
  import { tick } from "svelte";
  import { asHubFailure } from "./api";
  import { hub } from "./store.svelte";
  import { t } from "../lib/locale";

  /** The administrator's password goes to the hub only (decision 0034); the page keeps nothing. */
  const l = t.hub.login;
  let password = $state("");
  let busy = $state(false);
  let problem = $state<string | null>(null);
  let input: HTMLInputElement | undefined = $state();

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (busy) return;
    if (!password) {
      problem = l.empty;
      input?.focus();
      return;
    }
    busy = true;
    problem = null;
    try {
      await hub.login(password);
      password = "";
    } catch (err) {
      const f = asHubFailure(err);
      problem =
        f.status === 429 || f.code === "rate_limited"
          ? l.rateLimited
          : f.status === 401
            ? l.wrong
            : f.status === 0
              ? l.offline
              : l.failed(f.message);
      if (f.status === 401) password = "";
      await tick();
      input?.focus();
    } finally {
      busy = false;
    }
  }
</script>

<div class="login">
  <h1 class="lead">{l.lead}</h1>
  <p class="lead-text">{l.text}</p>
  {#if hub.ended}
    <p class="notice-box ended" role="status">{l.ended}</p>
  {/if}
  <form onsubmit={submit} novalidate>
    <label for="hub-pw">{l.password}</label>
    <div class="row">
      <input
        id="hub-pw"
        class="input"
        type="password"
        autocomplete="current-password"
        spellcheck="false"
        bind:this={input}
        bind:value={password}
        aria-invalid={problem && problem !== l.offline ? "true" : undefined}
        aria-describedby={problem ? "hub-pw-problem" : undefined}
      />
      <button class="btn primary" type="submit" aria-disabled={busy ? "true" : undefined}>{busy ? l.working : l.submit}</button>
    </div>
    <p class="hint bad" id="hub-pw-problem" role="alert">{problem ?? ""}</p>
  </form>
</div>

<style>
  .login { max-width: 520px; }
  .ended { margin-top: 16px; color: var(--muted); }
  form { margin-top: 22px; }
  label { display: block; font-weight: 500; font-size: 13px; margin-bottom: 4px; }
  .row { display: flex; gap: 8px; flex-wrap: wrap; }
  .input {
    flex: 1 1 240px; min-width: 0; background: var(--field); border: 1px solid var(--field-border); border-radius: 6px;
    padding: 7px 10px; min-height: 36px;
  }
  .input[aria-invalid="true"] { border-color: var(--bad); }
  @media (max-width: 760px) {
    .row .btn { flex: 1 1 100%; padding: 10px 12px; }
  }
</style>
