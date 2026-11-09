<script lang="ts">
  import { t } from "../lib/locale";

  /** A failed load is shown as such, never as empty data. */
  let { text, onretry }: { text: string; onretry: () => void } = $props();
</script>

<p class="load-error" role="status">
  <span>{text}</span>
  <button class="linkbtn" type="button" onclick={onretry}>{t.app.retry}</button>
</p>

<style>
  .load-error { color: var(--muted); padding: 10px 0; display: flex; flex-wrap: wrap; gap: 4px 12px; align-items: baseline; }
</style>
