<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import { app } from "../lib/store.svelte";
  import type { ApiFailure } from "../lib/api";
  import { t } from "../lib/locale";

  let { failure }: { failure: ApiFailure } = $props();
</script>

<p class="kicker">nestegg</p>
{#if failure.kind === "unauthorized"}
  <h1>{t.errors.authTitle}</h1>
  <p class="lead-text">{t.errors.authText}</p>
{:else if failure.kind === "daemon"}
  <h1>{t.errors.daemonTitle}</h1>
  <p class="lead-text">{t.errors.daemonText}</p>
  <Cmd command="systemctl status nestegg.service" />
  {#if failure.message}<p class="hint">{t.errors.detail(failure.message)}</p>{/if}
  <div class="actions"><button class="btn primary" type="button" onclick={() => app.retry()}>{t.app.retry}</button></div>
{:else}
  <h1>{t.errors.genericTitle}</h1>
  <p class="lead-text">{t.errors.genericText}</p>
  {#if failure.message}<p class="hint">{t.errors.detail(failure.message)}</p>{/if}
  <div class="actions"><button class="btn primary" type="button" onclick={() => app.retry()}>{t.app.retry}</button></div>
{/if}
