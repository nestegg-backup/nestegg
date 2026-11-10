<script lang="ts">
  import { hasPlaceholder } from "../lib/derive";
  import { t } from "../lib/locale";

  /**
   * A terminal command with a copy button. `label` puts a short lead-in in front ("Ändern im Terminal:").
   * A command that still contains a placeholder (`<NAME>`) cannot be copied: the button stays in the
   * tab order but does nothing (`aria-disabled`), and `missing` (or a general sentence) says why.
   */
  let { command, label, missing }: { command: string; label?: string; missing?: string } = $props();

  const incomplete = $derived(hasPlaceholder(command));
  // What the button copies, for its accessible name: the command on one line, not too long.
  const oneLine = $derived(command.replace(/\s*\\\n\s*/g, " "));
  const what = $derived(oneLine.length > 64 ? `${oneLine.slice(0, 63)}…` : oneLine);
  const uid = $props.id();
  let copyLabel = $state(t.app.copy);
  let announce = $state("");
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function copy() {
    if (incomplete) return;
    try {
      await navigator.clipboard.writeText(command);
      copyLabel = t.app.copied;
    } catch {
      copyLabel = t.app.copyFailed;
    }
    announce = copyLabel;
    clearTimeout(timer);
    timer = setTimeout(() => {
      copyLabel = t.app.copy;
      announce = "";
    }, 1600);
  }
</script>

<div class="cmd" class:labelled={!!label}>
  {#if label}<span class="label">{label}</span>{/if}
  <code>{command}</code>
  <button
    class="btn small"
    type="button"
    aria-label={t.app.copyOf(what)}
    aria-disabled={incomplete ? "true" : undefined}
    aria-describedby={incomplete ? `${uid}-why` : undefined}
    onclick={copy}>{copyLabel}</button
  >
  <!-- One live region, outside the button, so that "Copied" is announced once. -->
  <span class="sr" role="status">{announce}</span>
  {#if incomplete}<span class="why" id="{uid}-why">{missing ?? t.app.copyMissing}</span>{/if}
</div>

<style>
  .cmd {
    display: flex; align-items: center; gap: 10px; margin-top: 10px; min-width: 0; max-width: 640px;
    background: var(--side); border: 1px solid var(--rule); border-radius: 6px; padding: 6px 6px 6px 12px;
  }
  code { flex: 1; min-width: 0; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--text); }
  .btn { flex: none; }
  .why { flex: none; color: var(--muted); font-size: var(--fs-s); max-width: 16em; }
  .labelled { background: none; border: 0; padding: 0; max-width: none; flex-wrap: wrap; color: var(--muted); font-size: 13px; }
  .labelled code {
    flex: 0 1 auto; background: var(--box); border: 1px solid var(--rule); border-radius: 4px; padding: 1px 6px;
  }
  .labelled .btn { margin-left: auto; }
  @media (max-width: 760px) {
    .cmd:not(.labelled) { flex-wrap: wrap; }
    .cmd:not(.labelled) code { flex-basis: 100%; }
    .why { max-width: none; }
  }
</style>
