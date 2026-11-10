<script lang="ts">
  /** An on/off switch (`role="switch"`), labelled by the text of its settings row. */
  let {
    checked,
    labelledby,
    describedby,
    disabled = false,
    busy = false,
    onchange,
  }: {
    checked: boolean;
    labelledby: string;
    describedby?: string;
    disabled?: boolean;
    busy?: boolean;
    onchange: (next: boolean) => void;
  } = $props();
</script>

<button
  class="switch"
  type="button"
  role="switch"
  aria-checked={checked}
  aria-labelledby={labelledby}
  aria-describedby={describedby}
  aria-busy={busy || undefined}
  {disabled}
  onclick={() => onchange(!checked)}
></button>

<style>
  .switch {
    position: relative; flex: none; width: 42px; height: 24px; padding: 0; cursor: pointer;
    border-radius: 12px; border: 1px solid var(--field-border); background: var(--field);
  }
  .switch::after {
    content: ""; position: absolute; top: 3px; left: 3px; width: 16px; height: 16px; border-radius: 50%;
    background: var(--muted); transition: transform 0.12s;
  }
  .switch[aria-checked="true"] { background: var(--accent); border-color: var(--accent); }
  .switch[aria-checked="true"]::after { transform: translateX(18px); background: var(--on-accent); }
  .switch:focus-visible { outline-offset: 2px; border-radius: 12px; }
  .switch[disabled] { cursor: not-allowed; opacity: 0.55; }
  .switch[aria-busy="true"] { cursor: progress; }
</style>
