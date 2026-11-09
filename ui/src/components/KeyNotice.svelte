<script lang="ts">
  import Dot from "./Dot.svelte";
  import { t } from "../lib/locale";

  /** Another PC whose key copy is not confirmed. For this PC the guide on its page leads through the steps. */
  let { name }: { name: string } = $props();
</script>

<section class="keynote notice-box" aria-labelledby="h-key">
  <h2 id="h-key"><Dot tone="warn" />{t.key.otherTitle}</h2>
  <p>{t.key.otherText(name)}</p>
  <p>{t.key.otherFix(name)}</p>
</section>

<style>
  .keynote { margin-top: 22px; max-width: 720px; padding: 14px 16px 16px; }
  h2 { font-size: var(--fs-m); font-weight: 600; display: flex; align-items: center; gap: 10px; }
  p { color: var(--muted); margin-top: 6px; max-width: 66ch; }
</style>
