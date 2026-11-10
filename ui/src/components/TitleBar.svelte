<script lang="ts">
  import ThemeToggle from "./ThemeToggle.svelte";
  import { t } from "../lib/locale";

  /**
   * `heading`: the page name, the page's only h1 (null while there is no page).
   * `showBack`: the "‹ Alle PCs" control, visible on a phone only.
   */
  let { heading, showBack }: { heading: { title: string; sub: string | null } | null; showBack: boolean } = $props();
</script>

<header class="title" class:detail={showBack}>
  <div class="app-name">nestegg</div>
  <div class="bar">
    {#if showBack}
      <a class="back" href="#/">
        <svg viewBox="0 0 20 20" aria-hidden="true"><path d="M12.5 4 6.5 10l6 6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {t.nav.allPcs}
      </a>
    {/if}
    {#if heading}
      <h1 class="crumb">{heading.title}{#if heading.sub}{" "}<span class="sub">{heading.sub}</span>{/if}</h1>
    {:else}
      <span class="crumb"></span>
    {/if}
    <ThemeToggle />
  </div>
</header>

<style>
  .title {
    grid-column: 1 / -1; position: sticky; top: 0; z-index: 10;
    display: grid; grid-template-columns: var(--side-w) minmax(0, 1fr); height: var(--title-h);
    background: var(--win); border-bottom: 1px solid var(--rule);
  }
  :global(.app.bare) .title { grid-template-columns: auto minmax(0, 1fr); }
  :global(.app.bare) .app-name { background: none; border: 0; }
  .app-name {
    background: var(--side); border-right: 1px solid var(--rule-side);
    display: flex; align-items: center; padding: 0 18px; font-weight: 600; font-size: var(--fs-t);
  }
  .bar { display: flex; align-items: center; gap: 12px; padding: 0 20px 0 28px; min-width: 0; }
  .crumb {
    flex: 1; min-width: 0; margin: 0; font-size: var(--fs-t); font-weight: 600; line-height: 1.3; letter-spacing: 0;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .sub { color: var(--muted); font-weight: 400; }
  .back { display: none; }

  @media (max-width: 760px) {
    .title { display: flex; }
    .app-name { background: none; border: 0; padding: 0 0 0 16px; }
    .bar { flex: 1; padding: 0 10px 0 16px; }
    .detail .app-name { display: none; }
    .detail .bar { padding-left: 4px; gap: 4px; }
    .back {
      display: inline-flex; align-items: center; gap: 2px; flex: none;
      min-height: 44px; min-width: 44px; padding: 0 10px 0 4px; border-radius: 6px;
      color: var(--accent-text); font-weight: 600; text-decoration: none;
    }
    .back svg { width: 20px; height: 20px; }
    .back:hover { background: var(--box); }
    /* The list screen has the product name; a detail screen names its page next to the back control. */
    .title:not(.detail) .crumb { visibility: hidden; }
    .detail .crumb { padding-left: 6px; border-left: 1px solid var(--rule); }
    .detail .sub { display: none; }
    /* Room for the back control and the page name: the theme is chosen on the list screen. */
    .detail :global(.seg) { display: none; }
  }
</style>
