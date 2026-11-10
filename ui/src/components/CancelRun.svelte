<script lang="ts">
  import { tick } from "svelte";
  import { app } from "../lib/store.svelte";
  import { t } from "../lib/locale";

  const c = $derived(app.cancel);
  const sending = $derived(c.state === "sending");
  let root: HTMLElement | undefined = $state();
  let keep: HTMLButtonElement | undefined = $state();
  let ask: HTMLButtonElement | undefined = $state();
  let status: HTMLElement | undefined = $state();

  async function open() {
    app.cancel = { state: "asking" };
    await tick();
    keep?.focus();
  }

  /** Back to the button that opened the question; also what Escape does. */
  async function close() {
    if (app.cancel.state !== "asking") return;
    app.cancel = { state: "idle" };
    await tick();
    ask?.focus();
  }

  async function confirm() {
    // aria-disabled instead of disabled: the button keeps the focus while the request runs.
    if (app.cancel.state !== "asking") return;
    await app.cancelRun();
    await tick();
    // The question is gone now. Leave the focus alone if the user went elsewhere; otherwise it
    // would fall to the page, so it goes to the message about what happened.
    const a = document.activeElement;
    if (!a || a === document.body || root?.contains(a)) (status ?? ask)?.focus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape" && c.state === "asking") {
      e.preventDefault();
      void close();
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="cancel" bind:this={root}>
  {#if c.state === "asking" || c.state === "sending"}
    <div class="notice-box" role="group" aria-labelledby="cancel-q">
      <p class="q" id="cancel-q">{t.self.cancelAsk}</p>
      <p class="hint">{t.self.cancelAskText}</p>
      <div class="row">
        <button class="btn small" type="button" aria-disabled={sending ? "true" : undefined} onclick={confirm}>
          {sending ? t.self.cancelling : t.self.cancelYes}
        </button>
        <button class="btn small primary" type="button" bind:this={keep} aria-disabled={sending ? "true" : undefined} onclick={close}>
          {t.self.cancelNo}
        </button>
      </div>
    </div>
  {:else if c.state === "requested"}
    <p class="hint" role="status" tabindex="-1" bind:this={status}>{t.self.cancelling}</p>
  {:else}
    <button class="btn small" type="button" bind:this={ask} disabled={app.offline} onclick={open}>{t.self.cancel}</button>
    {#if c.state === "error"}
      <p class="hint" class:bad={c.kind === "other"} role="status" tabindex="-1" bind:this={status}>
        {c.kind === "gone" ? t.self.cancelGone : c.kind === "cannot" ? t.self.cancelCannot : t.self.cancelFailed(c.message)}
      </p>
    {/if}
  {/if}
</div>

<style>
  .cancel { margin-top: 14px; max-width: 520px; }
  .q { font-weight: 600; }
  .row { display: flex; gap: 8px; margin-top: 10px; flex-wrap: wrap; }
</style>
