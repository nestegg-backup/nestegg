<script lang="ts">
  import TitleBar from "../components/TitleBar.svelte";
  import Dot from "../components/Dot.svelte";
  import Login from "./Login.svelte";
  import Computers from "./Computers.svelte";
  import HubStatusBar from "./HubStatusBar.svelte";
  import { hub } from "./store.svelte";
  import { t } from "../lib/locale";

  hub.start();

  // The page name in the title bar (its only h1). The login page has its own h1.
  const heading = $derived(hub.phase === "in" && hub.hub ? { title: hub.hub.name, sub: t.hub.sub } : null);
  $effect(() => {
    document.title = hub.phase === "in" && hub.hub ? t.hub.pageTitle(hub.hub.name) : t.hub.pageTitle(t.hub.name);
  });
</script>

<!-- `bare`: the title bar of the local interface without a sidebar column. -->
<div class="app bare">
  <TitleBar {heading} showBack={false} />
  <main class="main" class:narrow={hub.phase !== "in"}>
    {#if hub.phase === "checking"}
      {#if hub.startError}
        <div class="head solo">
          <div class="status">
            <h1 class="lead"><Dot tone="warn" />{t.hub.login.offline}</h1>
            <p class="lead-text">{t.errors.detail(hub.startError.message)}</p>
          </div>
          <div class="acts"><button class="btn primary" type="button" onclick={() => hub.checkSession()}>{t.app.retry}</button></div>
        </div>
      {:else}
        <p class="lead-text" role="status">{t.hub.loading}</p>
      {/if}
    {:else if hub.phase === "login"}
      <Login />
    {:else}
      <Computers />
    {/if}
  </main>
  <HubStatusBar />
</div>

<style>
  .app {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: var(--title-h) 1fr auto;
    min-height: 100vh;
  }
  .main { min-width: 0; width: 100%; max-width: 1240px; padding: 24px 28px 48px; }
  .main.narrow { max-width: 760px; padding-top: 40px; }

  @media (max-width: 760px) {
    .app { display: block; }
    .main, .main.narrow { padding: 18px 16px calc(var(--sbar-h) + 32px); }
    .main.narrow { padding-top: 24px; }
    /* There is no list screen: the household name stays in the title bar (TitleBar hides it there). */
    .app :global(.title .crumb) { visibility: visible; }
  }
</style>
