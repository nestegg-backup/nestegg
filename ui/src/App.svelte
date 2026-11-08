<script lang="ts">
  import { tick } from "svelte";
  import ListPane from "./components/ListPane.svelte";
  import ThisPc from "./components/ThisPc.svelte";
  import OtherPc from "./components/OtherPc.svelte";
  import ServerView from "./components/ServerView.svelte";
  import SettingsView from "./components/SettingsView.svelte";
  import ErrorView from "./components/ErrorView.svelte";
  import ConnectionNotice from "./components/ConnectionNotice.svelte";
  import RestoreFlow from "./components/restore/RestoreFlow.svelte";
  import { app } from "./lib/store.svelte";
  import { router } from "./lib/router.svelte";
  import { machineName } from "./lib/derive";
  import { t } from "./lib/locale";

  app.start();

  const route = $derived(router.route);
  const focus = $derived(route.view === "restore" || route.view === "settings" || (!app.status && !!app.fatal));
  const machine = $derived(
    route.view === "pc" ? (app.machines?.machines.find((m) => m.backup_id === route.id) ?? null) : null,
  );

  const pageTitle = $derived.by(() => {
    switch (route.view) {
      case "server":
        return t.nav.server;
      case "settings":
        return t.nav.settings;
      case "restore":
        return t.restore.title;
      case "pc":
        return machine ? machineName(machine) : "nestegg";
      default:
        return app.status?.machine.hostname ?? "nestegg";
    }
  });
  $effect(() => {
    document.title = pageTitle === "nestegg" ? "nestegg" : t.app.title(pageTitle);
  });

  // Move to the top and put keyboard focus on the new page after navigation.
  let main: HTMLElement | undefined = $state();
  let lastKey = "";
  $effect(() => {
    const key = JSON.stringify(route);
    if (key === lastKey) return;
    const first = lastKey === "";
    lastKey = key;
    if (first) return;
    void tick().then(() => {
      window.scrollTo(0, 0);
      main?.focus({ preventScroll: true });
    });
  });
</script>

<div class="app" class:focus class:r-list={route.view === "home" && !focus}>
  {#if !focus}
    <nav class="pane-list" aria-label={t.nav.label}>
      <ListPane {route} />
    </nav>
  {/if}
  <main class="pane-main" tabindex="-1" bind:this={main}>
    <div class="main-inner">
      <ConnectionNotice />
      {#if !app.status}
        {#if app.fatal}
          <ErrorView failure={app.fatal} />
        {:else}
          <p class="lead-text" role="status">{t.app.loading}</p>
        {/if}
      {:else if route.view === "home" || route.view === "self"}
        <ThisPc status={app.status} />
      {:else if route.view === "pc"}
        <OtherPc {machine} id={route.id} />
      {:else if route.view === "server"}
        <ServerView status={app.status} />
      {:else if route.view === "settings"}
        <SettingsView status={app.status} />
      {:else if route.view === "restore"}
        <RestoreFlow status={app.status} step={route.step} />
      {/if}
    </div>
  </main>
</div>

<style>
  .app { display: grid; grid-template-columns: 288px minmax(0, 1fr); min-height: 100vh; }
  .pane-list { background: var(--bg1); }
  .pane-main { padding: 52px 56px 72px; min-width: 0; }
  .pane-main:focus { outline: none; }
  .main-inner { max-width: 680px; }
  .focus { grid-template-columns: minmax(0, 1fr); }
  .focus .pane-main { padding-top: 40px; }
  .focus .main-inner { margin: 0 auto; max-width: 640px; }

  @media (max-width: 760px) {
    .app { grid-template-columns: minmax(0, 1fr); }
    .pane-list { min-height: 100vh; }
    .pane-main { padding: 18px 16px 56px; }
    .focus .pane-main { padding-top: 18px; }
    .r-list .pane-main { display: none; }
    .app:not(.r-list) .pane-list { display: none; }
  }
</style>
