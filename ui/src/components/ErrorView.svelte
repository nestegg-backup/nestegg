<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import type { ApiFailure } from "../lib/api";
  import { t } from "../lib/locale";

  /** The page when the daemon gave no first answer: the title bar has no page name, so this heading is the h1. */
  let { failure }: { failure: ApiFailure } = $props();
</script>

<div class="head solo">
  <div class="status">
    {#if failure.kind === "unauthorized"}
      <h1 class="lead"><Dot tone="warn" />{t.errors.authTitle}</h1>
      <p class="lead-text">{t.errors.authText}</p>
    {:else if failure.kind === "daemon"}
      <h1 class="lead"><Dot tone="warn" />{t.errors.daemonTitle}</h1>
      <p class="lead-text">{t.errors.daemonText}</p>
      <Cmd command="systemctl status nestegg.service" />
      {#if failure.message}<p class="hint">{t.errors.detail(failure.message)}</p>{/if}
    {:else}
      <h1 class="lead"><Dot tone="bad" />{t.errors.genericTitle}</h1>
      <p class="lead-text">{t.errors.genericText}</p>
      {#if failure.message}<p class="hint">{t.errors.detail(failure.message)}</p>{/if}
    {/if}
  </div>
  {#if failure.kind !== "unauthorized"}
    <div class="acts"><button class="btn primary" type="button" onclick={() => app.retry()}>{t.app.retry}</button></div>
  {/if}
</div>
