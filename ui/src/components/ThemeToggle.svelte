<script lang="ts">
  import { applyTheme, readTheme, storeTheme, THEME_CHOICES, type ThemeChoice } from "../lib/theme";
  import { t } from "../lib/locale";

  let choice = $state<ThemeChoice>(readTheme());

  function pick(c: ThemeChoice) {
    choice = c;
    storeTheme(c);
    applyTheme(c, document.documentElement);
  }

  // public/theme.js applied the stored choice before the first paint; keep <html> in step.
  $effect(() => applyTheme(choice, document.documentElement));
</script>

<div class="seg" role="group" aria-label={t.theme.label}>
  {#each THEME_CHOICES as c (c)}
    <button type="button" aria-pressed={choice === c} onclick={() => pick(c)}>{t.theme[c]}</button>
  {/each}
</div>

<style>
  .seg { display: inline-flex; flex: none; background: var(--box); border: 1px solid var(--rule); border-radius: 7px; padding: 2px; }
  button {
    appearance: none; border: 0; background: none; cursor: pointer;
    padding: 3px 10px; border-radius: 5px; font-size: var(--fs-s); color: var(--muted); line-height: 1.5;
  }
  button:hover { color: var(--text); }
  button[aria-pressed="true"] { background: var(--win); color: var(--text); box-shadow: 0 0 0 1px var(--rule); }
  @media (max-width: 760px) {
    button { padding: 6px 10px; }
  }
</style>
