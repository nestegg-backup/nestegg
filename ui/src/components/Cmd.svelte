<script lang="ts">
  import { t } from "../lib/locale";

  /** A terminal command with a copy button. `label` puts a short lead-in in front ("Ändern im Terminal:"). */
  let { command, label }: { command: string; label?: string } = $props();
  let copyLabel = $state(t.app.copy);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      copyLabel = t.app.copied;
    } catch {
      copyLabel = t.app.copyFailed;
    }
    clearTimeout(timer);
    timer = setTimeout(() => (copyLabel = t.app.copy), 1600);
  }
</script>

<div class="cmd" class:labelled={!!label}>
  {#if label}<span class="label">{label}</span>{/if}
  <code>{command}</code>
  <button class="btn small" type="button" onclick={copy}><span aria-live="polite">{copyLabel}</span></button>
</div>

<style>
  .cmd {
    display: flex; align-items: center; gap: 10px; margin-top: 10px; min-width: 0; max-width: 640px;
    background: var(--side); border: 1px solid var(--rule); border-radius: 6px; padding: 6px 6px 6px 12px;
  }
  code { flex: 1; min-width: 0; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--text); }
  .btn { flex: none; }
  .labelled { background: none; border: 0; padding: 0; max-width: none; flex-wrap: wrap; color: var(--muted); font-size: 13px; }
  .labelled code {
    flex: 0 1 auto; background: var(--box); border: 1px solid var(--rule); border-radius: 4px; padding: 1px 6px;
  }
  .labelled .btn { margin-left: auto; }
  @media (max-width: 760px) {
    .cmd:not(.labelled) { flex-wrap: wrap; }
    .cmd:not(.labelled) code { flex-basis: 100%; }
  }
</style>
