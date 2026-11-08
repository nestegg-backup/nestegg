<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import Dot from "./Dot.svelte";
  import { t } from "../lib/locale";

  /** Without a name: this PC (fixable here). With a name: another PC. */
  let { name }: { name?: string } = $props();
</script>

<div class="keynote">
  <h3><Dot tone="warn" />{name ? t.key.otherTitle : t.key.title}</h3>
  <p>{name ? t.key.otherText(name) : t.key.text}</p>
  <p>{name ? t.key.otherFix(name) : t.key.steps}</p>
  <Cmd command="sudo nestegg key paperkey" />
  {#if !name}<p class="hint">{t.key.paperkey}</p>{/if}
  <Cmd command="sudo nestegg key confirm-exported" />
  {#if !name}<p class="hint">{t.key.confirm}</p>{/if}
</div>

<style>
  .keynote { margin-top: 40px; }
  h3 { font-size: var(--fs-m); font-weight: 600; display: flex; align-items: center; gap: 10px; }
  p { color: var(--muted); margin-top: 6px; max-width: 60ch; }
  p.hint { margin-top: 8px; }
</style>
