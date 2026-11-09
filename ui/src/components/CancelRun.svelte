<script lang="ts">
  import { tick } from "svelte";
  import { app } from "../lib/store.svelte";
  import { t } from "../lib/locale";

  const c = $derived(app.cancel);
  let keep: HTMLButtonElement | undefined = $state();
  let ask: HTMLButtonElement | undefined = $state();

  async function open() {
    app.cancel = { state: "asking" };
    await tick();
    keep?.focus();
  }

  async function close() {
    app.cancel = { state: "idle" };
    await tick();
    ask?.focus();
  }
</script>

<div class="cancel">
  {#if c.state === "asking"}
    <div class="notice-box" role="group" aria-labelledby="cancel-q">
      <p class="q" id="cancel-q">{t.self.cancelAsk}</p>
      <p class="hint">{t.self.cancelAskText}</p>
      <div class="row">
        <button class="btn small" type="button" onclick={() => app.cancelRun()}>{t.self.cancelYes}</button>
        <button class="btn small primary" type="button" bind:this={keep} onclick={close}>{t.self.cancelNo}</button>
      </div>
    </div>
  {:else if c.state === "sending" || c.state === "requested"}
    <p class="hint" role="status">{t.self.cancelling}</p>
  {:else}
    <button class="btn small" type="button" bind:this={ask} disabled={app.offline} onclick={open}>{t.self.cancel}</button>
    {#if c.state === "error"}
      <p class="hint" class:bad={c.kind === "other"} role="status">
        {c.kind === "cannot_cancel" ? t.self.cancelCannot : c.kind === "gone" ? t.self.cancelGone : t.self.cancelFailed(c.message)}
      </p>
    {/if}
  {/if}
</div>

<style>
  .cancel { margin-top: 14px; max-width: 520px; }
  .q { font-weight: 600; }
  .row { display: flex; gap: 8px; margin-top: 10px; flex-wrap: wrap; }
</style>
