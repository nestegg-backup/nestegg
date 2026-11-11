<script lang="ts">
  import { untrack } from "svelte";
  import Cmd from "../Cmd.svelte";
  import { app } from "../../lib/store.svelte";
  import { restore, stickNotSupported } from "../../lib/restore.svelte";
  import { href } from "../../lib/router.svelte";
  import { allDamaged, hasPbs, hasStick, isDamaged, restorableArchives, stickView } from "../../lib/derive";
  import { f, t } from "../../lib/locale";
  import type { Snapshot, Status } from "../../lib/types";

  let { status }: { status: Status } = $props();

  const NEWEST = 7;
  // Server and stick: the user picks where the files come from. Otherwise there is nothing to pick.
  const both = $derived(hasPbs(status) && hasStick(status));
  const fromStick = $derived(restore.source === "stick");
  const stickTs = $derived(status.targets?.stick ?? null);
  const plugged = $derived(stickTs ? stickView(stickTs).plugged : null);
  const absent = $derived(fromStick && plugged === false);
  const snaps = $derived(restore.snaps(app.snapshots));
  const list = $derived(snaps.list);
  const newest = $derived(list.slice(0, NEWEST));
  const older = $derived(list.slice(NEWEST));
  const olderGroups = $derived.by(() => {
    const groups: { title: string; items: Snapshot[] }[] = [];
    for (const s of older) {
      const title = f.monthYear(s.time);
      const g = groups[groups.length - 1];
      if (g && g.title === title) g.items.push(s);
      else groups.push({ title, items: [s] });
    }
    return groups;
  });
  // Marked as damaged by the target's own check (the legacy field is the server's when there is one).
  const failed = $derived((fromStick ? stickTs?.server_verify_failed : status.server_verify_failed) ?? []);
  const damaged = (s: Snapshot) => isDamaged(s, failed);
  // Nothing else to pick: then "take another one" would be no advice.
  const everyDamaged = $derived(allDamaged(list, failed));
  const current = $derived(restore.snapshot(list));
  const showOlder = $derived(restore.showOlder || (!!current && older.includes(current)));

  // Default to the newest snapshot, and open its first archive right away:
  // that is what step 2 shows first, and it tells whether browsing works at all.
  // The stick is opened only when it is the source and not known to be missing.
  $effect(() => {
    if (fromStick && plugged !== false && restore.stickSnaps.state === "idle") untrack(() => void restore.loadStickSnapshots());
  });

  async function retryStick() {
    // The status says whether the stick is plugged in; it is asked only every half minute.
    await app.poll();
    void restore.loadStickSnapshots(true);
  }

  $effect(() => {
    if (absent || snaps.state !== "ok" || !list.length) return;
    // The newest snapshot that is not marked as damaged.
    if (!current) restore.pick((list.find((x) => !isDamaged(x, failed)) ?? list[0]!).time);
    const s = restore.snapshot(list);
    if (s) untrack(() => void restore.prepare(s, app.config.value));
  });

  const cliExample = $derived.by(() => {
    const s = list[0];
    if (!s) return "";
    const archive = restorableArchives(s.archives)[0] ?? "home.pxar";
    return `sudo nestegg restore ${s.snapshot} ${archive} ${t.restore.notAvailableTarget}`;
  });
</script>

{#snippet choice(s: Snapshot)}
  <label class="choice">
    <input type="radio" name="snap" value={s.time} checked={restore.snapshotTime === s.time} onchange={() => restore.pick(s.time)} />
    <span>{f.dayLongYear(s.time)}</span>
    <span class="side">{#if damaged(s)}<span class="dmg">{t.restore.damaged}</span><span class="sr">, </span>{/if}{f.time(s.time)}</span>
  </label>
{/snippet}

{#if both}
  <fieldset class="choice-group">
    <legend class="sec-title">{t.restore.sourceTitle}</legend>
    <div class="box choices">
      <label class="choice">
        <input type="radio" name="source" value="pbs" checked={!fromStick} onchange={() => restore.setSource("pbs")} />
        <span>{t.stick.server}</span>
        <span class="side where">{status.pbs.server}</span>
      </label>
      <label class="choice">
        <input type="radio" name="source" value="stick" checked={fromStick} onchange={() => restore.setSource("stick")} />
        <span>{t.stick.name}</span>
        <span class="side">{plugged === true ? t.restore.stickIn : plugged === false ? t.restore.stickOut : ""}</span>
      </label>
    </div>
  </fieldset>
{/if}

{#if absent}
  <p class="lead-text gap" role="status">{t.restore.stickAbsent}</p>
  <div class="actions"><button class="btn" type="button" onclick={retryStick}>{t.app.retry}</button></div>
{:else if snaps.state === "loading" || snaps.state === "idle"}
  <p class="lead-text gap" role="status">{fromStick ? t.restore.loadingStick : t.restore.loadingSnapshots}</p>
{:else if snaps.state === "error" && fromStick}
  {#if stickNotSupported(snaps.error)}
    <p class="lead-text gap">{t.restore.stickNotSupported}</p>
  {:else}
    <p class="lead-text gap">{snaps.error?.kind === "stick_unavailable" ? t.restore.stickUnavailable : t.errors.genericText}</p>
    {#if snaps.error?.message}<p class="hint">{t.errors.detail(snaps.error.message)}</p>{/if}
    <div class="actions"><button class="btn primary" type="button" onclick={retryStick}>{t.app.retry}</button></div>
  {/if}
{:else if snaps.state === "error"}
  {#if snaps.error?.kind === "pbs_unreachable"}
    <p class="lead-text gap">{t.restore.pbsUnreachable}{status.pbs.wake_on_lan ? ` ${t.restore.pbsUnreachableWol}` : ""}</p>
  {:else}
    <p class="lead-text gap">{t.errors.genericText}</p>
    {#if snaps.error?.message}<p class="hint">{t.errors.detail(snaps.error.message)}</p>{/if}
  {/if}
  <div class="actions"><button class="btn primary" type="button" onclick={() => app.loadSnapshots(true)}>{t.app.retry}</button></div>
{:else if !list.length}
  <p class="lead-text gap">{fromStick ? t.restore.noSnapshotsStick : t.restore.noSnapshots}</p>
{:else if restore.browse === "not_implemented" && fromStick}
  <p class="lead-text gap">{t.restore.stickNotSupported}</p>
{:else if restore.browse === "not_implemented"}
  <p class="lead-text gap">{t.restore.notAvailable}</p>
  <Cmd command={cliExample} />
{:else}
  <p class="lead-text gap">{t.restore.pickIntro}</p>
  <fieldset class="choice-group">
    <legend class="sec-title">{t.restore.newest}</legend>
    <div class="box choices">
      {#each newest as s (s.time)}{@render choice(s)}{/each}
    </div>
  </fieldset>
  {#if showOlder}
    {#each olderGroups as g (g.title)}
      <fieldset class="choice-group">
        <legend class="sec-title">{g.title}</legend>
        <div class="box choices">
          {#each g.items as s (s.time)}{@render choice(s)}{/each}
        </div>
      </fieldset>
    {/each}
  {/if}
  {#if older.length && !showOlder}
    <p class="older">
      <button class="btn small" type="button" onclick={() => (restore.showOlder = true)}>
        {t.restore.older(older.length, f.monthYear(older[older.length - 1]!.time))}
      </button>
    </p>
  {/if}
  {#if current && damaged(current)}
    <p class="hint bad" role="status">
      {fromStick
        ? everyDamaged ? t.restore.damagedAllStick : t.restore.damagedSnapshotStick
        : everyDamaged ? t.restore.damagedAll : t.restore.damagedSnapshot}
    </p>
  {/if}
  <div class="selbar">
    <span class="muted">{current ? f.dayLong(current.time) : ""}</span>
    <a class="btn primary" href={href({ view: "restore", step: "2" })} aria-disabled={current ? undefined : "true"}>{t.restore.showFiles}</a>
  </div>
{/if}

<style>
  .gap { margin-top: 20px; }
  .actions { margin-top: 14px; }
  fieldset { border: 0; margin: 22px 0 0; padding: 0; min-width: 0; }
  legend { padding: 0; }
  .choices { margin-top: 0; overflow: hidden; }
  .choice {
    display: grid; grid-template-columns: 18px minmax(0, 1fr) auto; gap: 12px; align-items: center;
    padding: 9px 14px; cursor: pointer; border-top: 1px solid var(--rule);
  }
  .choice:first-child { border-top: 0; }
  .choice:hover { background: var(--box-hover); }
  .choice:has(input:checked) { background: var(--sel); }
  .choice input { margin: 0; width: 16px; height: 16px; accent-color: var(--accent); }
  .side { color: var(--muted); font-size: var(--fs-s); text-align: right; }
  .older { margin-top: 12px; }
  .where { overflow-wrap: anywhere; }
  .dmg { color: var(--bad); font-weight: 500; margin-right: 12px; }
</style>
