<script lang="ts">
  import { tick } from "svelte";
  import TitleBar from "./components/TitleBar.svelte";
  import Sidebar from "./components/Sidebar.svelte";
  import StatusBar from "./components/StatusBar.svelte";
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
  const status = $derived(app.status);
  // The daemon gave no first answer: there is nothing to list, the error takes the whole width.
  const bare = $derived(!status && !!app.fatal);
  // Below 760px list and detail are two screens; the list is the start page.
  const screen = $derived(route.view === "home" && !bare ? "list" : "detail");
  const machine = $derived(
    route.view === "pc" ? (app.machines?.machines.find((m) => m.backup_id === route.id) ?? null) : null,
  );

  /** Page name in the title bar (the only h1), with a quiet addition. */
  const heading = $derived.by((): { title: string; sub: string | null } | null => {
    if (!status) return null;
    switch (route.view) {
      case "server":
        return { title: t.nav.server, sub: status.pbs.server };
      case "settings":
        return { title: t.settings.title, sub: null };
      case "restore":
        return { title: t.restore.title, sub: status.machine.hostname };
      case "pc":
        return { title: machine ? machineName(machine) : route.id, sub: t.title.otherSub };
      default:
        return { title: status.machine.hostname, sub: t.title.selfSub(status.machine.os ?? null) };
    }
  });
  $effect(() => {
    document.title = heading ? t.app.title(heading.title) : "nestegg";
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

<div class="app" class:bare data-screen={screen}>
  <TitleBar {heading} showBack={!bare && screen === "detail"} />
  {#if !bare}
    <nav class="side" aria-label={t.nav.label}>
      <Sidebar {route} />
    </nav>
  {/if}
  <main class="main" tabindex="-1" bind:this={main}>
    <ConnectionNotice />
    {#if !status}
      {#if app.fatal}
        <ErrorView failure={app.fatal} />
      {:else}
        <p class="lead-text" role="status">{t.app.loading}</p>
      {/if}
    {:else if route.view === "home" || route.view === "self"}
      <ThisPc {status} />
    {:else if route.view === "pc"}
      <OtherPc {machine} id={route.id} />
    {:else if route.view === "server"}
      <ServerView {status} />
    {:else if route.view === "settings"}
      <SettingsView {status} />
    {:else if route.view === "restore"}
      <RestoreFlow {status} step={route.step} />
    {/if}
  </main>
  <StatusBar />
</div>

<style>
  .app {
    display: grid;
    grid-template-columns: var(--side-w) minmax(0, 1fr);
    grid-template-rows: var(--title-h) 1fr auto;
    min-height: 100vh;
  }
  .app.bare { grid-template-columns: minmax(0, 1fr); }
  .side { background: var(--side); border-right: 1px solid var(--rule-side); min-width: 0; }
  .main { min-width: 0; padding: 24px 28px 48px; }
  .main:focus { outline: none; }
  .bare .main { max-width: 760px; padding-top: 40px; }

  @media (max-width: 760px) {
    .app { display: block; }
    .side { border: 0; min-height: calc(100vh - var(--title-h) - var(--sbar-h)); }
    .main { padding: 18px 16px calc(var(--sbar-h) + 32px); }
    .bare .main { padding-top: 24px; }
    .app[data-screen="detail"] .side { display: none; }
    .app[data-screen="list"] .main { display: none; }
  }
</style>
