<script lang="ts">
  import { t } from "../lib/locale";

  let { command }: { command: string } = $props();
  let label = $state(t.app.copy);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      label = t.app.copied;
    } catch {
      label = t.app.copyFailed;
    }
    clearTimeout(timer);
    timer = setTimeout(() => (label = t.app.copy), 1600);
  }
</script>

<div class="cmd">
  <code>{command}</code>
  <button class="btn" type="button" onclick={copy}><span aria-live="polite">{label}</span></button>
</div>

<style>
  .cmd {
    display: flex; align-items: center; gap: 12px; margin-top: 14px;
    background: var(--bg2); border-radius: 8px; padding: 10px 10px 10px 14px; min-width: 0;
  }
  code { flex: 1; min-width: 0; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--text); padding: 2px 0; }
  .btn { padding: 6px 12px; background: var(--bg1); font-size: var(--fs-s); flex: none; }
  .btn:hover { background: var(--bg0); }
  @media (max-width: 760px) {
    .cmd { flex-wrap: wrap; }
    code { flex-basis: 100%; }
  }
</style>
