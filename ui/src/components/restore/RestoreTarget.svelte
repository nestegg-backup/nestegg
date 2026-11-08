<script lang="ts">
  import { app } from "../../lib/store.svelte";
  import { displayPath, folderNameFor, folderProblem, restore } from "../../lib/restore.svelte";
  import { href, router } from "../../lib/router.svelte";
  import { isInventoryArchive } from "../../lib/derive";
  import { f, t } from "../../lib/locale";

  const snap = $derived(restore.snapshot(app.snapshots.list));
  // Without the proposed `session` endpoint the UI does not know the user's home folder.
  const home = $derived(app.session ? app.session.home : null);
  const fullPath = $derived(!home);

  $effect(() => {
    if (snap && !restore.folder) restore.folder = folderNameFor(snap.time);
  });

  const value = $derived(fullPath ? restore.fullPath : restore.folder);
  const problem = $derived(folderProblem(value, fullPath));
  const problemText = $derived(
    problem === "empty"
      ? t.restore.folderEmpty
      : problem === "slash"
        ? t.restore.folderSlash
        : problem === "not_absolute"
          ? t.restore.pathNotAbsolute
          : problem === "invalid"
            ? t.restore.folderInvalid
            : null,
  );
  const startProblem = $derived(
    restore.startError
      ? restore.startError.exists
        ? t.restore.targetExists
        : restore.startError.notImplemented
          ? t.errors.notImplemented
          : t.restore.startFailed(restore.startError.message)
      : null,
  );
  const total = $derived(restore.totalSize());

  function label(sel: (typeof restore.selection)[number]) {
    return isInventoryArchive(sel.archive) && sel.path === "/" ? t.restore.inventory : displayPath(sel, app.config.value);
  }

  async function start() {
    if (problem) return;
    if (await restore.start(home)) router.go({ view: "restore", step: "done" });
  }
</script>

{#if snap}
  <p class="lead-text gap">{t.restore.targetIntro}</p>
  <div class="field">
    {#if fullPath}
      <label class="field-label" for="target">{t.restore.pathLabel}</label>
      <input
        class="input mono"
        id="target"
        autocomplete="off"
        spellcheck="false"
        placeholder="/home/…/{folderNameFor(snap.time)}"
        bind:value={restore.fullPath}
        oninput={() => (restore.startError = null)}
        aria-describedby="target-hint"
        aria-invalid={!!(problemText && restore.fullPath) || !!startProblem}
      />
    {:else}
      <label class="field-label" for="target">{t.restore.folderLabel}</label>
      <div class="prefix">
        <code>{home!.replace(/\/$/, "")}/</code>
        <input
          class="input mono"
          id="target"
          autocomplete="off"
          spellcheck="false"
          bind:value={restore.folder}
          oninput={() => (restore.startError = null)}
          aria-describedby="target-hint"
          aria-invalid={!!problemText || !!startProblem}
        />
      </div>
    {/if}
    <p class="hint" class:bad={!!startProblem || (!!problemText && value !== "")} id="target-hint" aria-live="polite">
      {startProblem ?? problemText ?? (fullPath ? t.restore.pathOk : t.restore.folderOk(app.session!.user))}
    </p>
  </div>

  <section class="summary" aria-labelledby="h-sum">
    <h2 id="h-sum">{t.restore.summary}</h2>
    <dl class="facts">
      <dt>{t.restore.sumSnapshot}</dt>
      <dd>{f.dayLongYear(snap.time)}, {f.time(snap.time)}</dd>
      <dt>{t.restore.sumSelection}</dt>
      <dd>
        <ul>
          {#each restore.selection as sel (sel.archive + sel.path)}<li><code>{label(sel)}</code></li>{/each}
        </ul>
      </dd>
      {#if total != null}
        <dt>{t.restore.sumSize}</dt>
        <dd>{f.size(total)}</dd>
      {/if}
      <dt>{t.restore.sumTarget}</dt>
      <dd><code>{restore.targetDir(home) || "–"}</code></dd>
    </dl>
  </section>

  <div class="selbar">
    <a class="linkbtn" href={href({ view: "restore", step: "2" })}>{t.restore.changeSelection}</a>
    <button class="btn primary" type="button" disabled={!!problem || restore.starting || app.offline} onclick={start}>
      {restore.starting ? t.restore.starting : t.restore.start}
    </button>
  </div>
{/if}

<style>
  .gap { margin-top: 24px; }
  .field { margin-top: 28px; }
  .field-label { display: block; font-weight: 500; margin-bottom: 8px; }
  .input {
    width: 100%; background: var(--bg2); border: 1px solid var(--field-border); border-radius: 8px;
    padding: 10px 12px; outline: none;
  }
  .input:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
  .input.mono { font-family: var(--mono); font-size: var(--fs-s); padding: 12px; }
  .prefix { display: flex; align-items: stretch; min-width: 0; }
  .prefix code {
    display: flex; align-items: center; padding: 0 2px 0 12px; background: var(--bg2);
    border: 1px solid var(--field-border); border-right: 0; border-radius: 8px 0 0 8px; color: var(--muted); white-space: nowrap;
  }
  .prefix .input { border-left: 0; border-radius: 0 8px 8px 0; padding-left: 2px; min-width: 0; }
  .prefix:focus-within { outline: 2px solid var(--accent); outline-offset: 1px; border-radius: 8px; }
  .prefix .input:focus-visible { outline: none; }
  .summary { margin-top: 30px; }
  .summary :global(.facts) { grid-template-columns: 140px minmax(0, 1fr); }
  .summary ul { display: grid; gap: 2px; }
  @media (max-width: 760px) {
    .summary :global(.facts) { grid-template-columns: minmax(0, 1fr); }
  }
</style>
