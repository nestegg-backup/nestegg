<script lang="ts">
  import { untrack } from "svelte";
  import Cmd from "./Cmd.svelte";
  import LoadError from "./LoadError.svelte";
  import Switch from "./Switch.svelte";
  import { app } from "../lib/store.svelte";
  import { reveal } from "../lib/reveal";
  import { href } from "../lib/router.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { ApiFailure } from "../lib/api";
  import { scheduleHours } from "../lib/derive";
  import { DEFAULT_LIMITS, settingsProblem } from "../lib/settings";
  import { intervalOptions, overdueFor, parseHours, problemText, saveFailure } from "../lib/settingsForm";
  import { f, t } from "../lib/locale";
  import type { SettingKey, SettingsChange, Status } from "../lib/types";

  let { status }: { status: Status } = $props();

  type Row = "automatic" | "interval" | "overdue" | "excludes" | "wol";
  /** `offline`: refused because the daemon did not answer; cleared when it is back. */
  type Problem = { text: string; detail: string | null; offline?: boolean };

  const s = $derived(t.settings);
  const c = $derived(app.config.value);
  const configFailed = $derived(app.config.state === "error");
  const unconfigured = $derived(status.health === "unconfigured");
  const editable = $derived(new Set<SettingKey>(c?.editable ?? []));
  const canEdit = $derived(editable.size > 0);
  const can = (k: SettingKey) => editable.has(k) && !app.offline;
  const limits = $derived(c?.limits ?? DEFAULT_LIMITS);

  const interval = $derived(scheduleHours(c, status));
  const overdue = $derived(c?.overdue_after_hours ?? status.overdue_after_hours ?? null);
  const automatic = $derived(c?.automatic_backups ?? status.schedule.enabled);
  const wol = $derived(c?.wake_on_lan ?? !!status.pbs.wake_on_lan);
  const wolConfigured = $derived(c?.wake_on_lan_configured ?? null);
  const excludes = $derived(c?.excludes ?? []);
  const sources = $derived((c?.sources ?? []).filter((x) => x.archive !== "inventory.pxar").map((x) => x.path));
  const current = $derived({
    interval_hours: interval ?? 24,
    overdue_after_hours: overdue ?? 36,
    wake_on_lan_configured: wolConfigured ?? false,
  });

  // ---- per-row state: saving, just saved, not saved
  let pending = $state<Partial<Record<Row, boolean>>>({});
  let saved = $state<Partial<Record<Row, boolean>>>({});
  let problems = $state<Partial<Record<Row, Problem>>>({});
  /** Value shown while a switch change is being saved. */
  let optimistic = $state<{ automatic?: boolean; wol?: boolean }>({});
  /** The overdue threshold was raised together with a longer rhythm. */
  let raised = $state<number | null>(null);
  const timers: Partial<Record<Row, ReturnType<typeof setTimeout>>> = {};

  let overdueRaw = $state("");
  let pattern = $state("");
  let patternInput: HTMLInputElement | undefined = $state();

  // Show the saved threshold unless the field holds a value that was refused.
  $effect(() => {
    const v = overdue;
    untrack(() => {
      if (!problems.overdue) overdueRaw = v == null ? "" : String(v);
    });
  });

  // The daemon answers again: drop the "does not answer" notes.
  $effect(() => {
    if (app.offline) return;
    untrack(() => {
      for (const row of Object.keys(problems) as Row[]) if (problems[row]?.offline) problems[row] = undefined;
    });
  });

  function flash(row: Row) {
    saved[row] = true;
    clearTimeout(timers[row]);
    timers[row] = setTimeout(() => (saved[row] = false), 2500);
  }

  const rowName: Record<Row, () => string> = {
    automatic: () => s.automatic,
    interval: () => s.interval,
    overdue: () => s.overdue,
    excludes: () => s.excludes,
    wol: () => s.wol,
  };

  /** Mark the row; the status bar names the setting, the row says why. */
  function refuse(row: Row, problem: Problem) {
    problems[row] = problem;
    saved[row] = false;
    statusBar.show(t.sbar.notSaved(rowName[row]()), "bad");
  }

  /** Check like the daemon, then send. Returns true when saved. */
  async function save(row: Row, change: SettingsChange, summary: string, alsoFlash: Row[] = []): Promise<boolean> {
    const local = settingsProblem(change, current, limits);
    if (local) {
      refuse(row, { text: problemText(local, t) ?? local, detail: null });
      return false;
    }
    pending[row] = true;
    problems[row] = undefined;
    try {
      await app.saveSettings(change);
      for (const r of [row, ...alsoFlash]) flash(r);
      statusBar.show(t.sbar.saved(f.time(new Date()), summary));
      return true;
    } catch (e) {
      const failure = e instanceof ApiFailure ? e : new ApiFailure("other", 0, null, String(e));
      refuse(row, { ...saveFailure(failure, t), offline: failure.kind === "daemon" });
      return false;
    } finally {
      pending[row] = false;
    }
  }

  async function setAutomatic(on: boolean) {
    optimistic.automatic = on;
    await save("automatic", { automatic_backups: on }, s.sumAutomatic(on));
    optimistic.automatic = undefined;
  }

  async function setWol(on: boolean) {
    optimistic.wol = on;
    await save("wol", { wake_on_lan: on }, s.sumWol(on));
    optimistic.wol = undefined;
  }

  async function changeInterval(e: Event & { currentTarget: HTMLSelectElement }) {
    const select = e.currentTarget;
    const hours = Number(select.value);
    if (!Number.isInteger(hours) || hours === interval) return;
    const raise = overdue != null ? overdueFor(hours, overdue, limits) : null;
    const label = s.everyHours(hours);
    const ok = raise
      ? await save("interval", { interval_hours: hours, overdue_after_hours: raise }, s.sumIntervalOverdue(label, raise), ["overdue"])
      : await save("interval", { interval_hours: hours }, s.sumInterval(label));
    if (ok) {
      raised = raise;
      if (raise) {
        problems.overdue = undefined;
        overdueRaw = String(raise);
      }
    } else if (interval != null) {
      select.value = String(interval);
    }
  }

  async function commitOverdue() {
    const hours = parseHours(overdueRaw);
    if (hours === null) return refuse("overdue", { text: s.problem.wholeNumber, detail: null });
    if (hours === overdue) {
      problems.overdue = undefined;
      return;
    }
    raised = null;
    await save("overdue", { overdue_after_hours: hours }, s.sumOverdue(hours));
  }

  async function freshExcludes(): Promise<string[]> {
    await app.loadConfig();
    return app.config.value?.excludes ?? excludes;
  }

  async function addPattern(e: SubmitEvent) {
    e.preventDefault();
    const p = pattern.trim();
    if (!p) return refuse("excludes", { text: s.problem.excludeEmpty, detail: null });
    // Build the list from the daemon's current one, not a copy loaded long ago:
    // it may have changed in another tab or in the terminal.
    const list = await freshExcludes();
    if (list.includes(p)) return refuse("excludes", { text: s.problem.excludeDuplicate, detail: null });
    const ok = await save("excludes", { excludes: [...list, p] }, s.sumExcludeAdded(p));
    // Keep whatever was typed meanwhile.
    if (ok && pattern.trim() === p) pattern = "";
  }

  async function removePattern(p: string) {
    // The button goes away with the pattern: keep the keyboard in the list.
    patternInput?.focus();
    const list = await freshExcludes();
    await save("excludes", { excludes: list.filter((x) => x !== p) }, s.sumExcludeRemoved(p));
  }

  const describe = (...ids: (string | false | null | undefined)[]) => ids.filter(Boolean).join(" ") || undefined;
</script>

{#snippet savedMark(row: Row)}
  {#if pending[row]}
    <span class="saving" aria-hidden="true">{s.saving}</span>
  {:else if saved[row]}
    <!-- The status bar announces it; this is the visual confirmation next to the control. -->
    <span class="saved" aria-hidden="true">{s.saved}</span>
  {/if}
{/snippet}

{#snippet problemLine(row: Row, id: string)}
  {#if problems[row]}
    <p class="e" id={id} use:reveal>
      {s.notSaved(problems[row]!.text)}
      {#if problems[row]!.detail}<span class="detail">{t.errors.detail(problems[row]!.detail!)}</span>{/if}
    </p>
  {/if}
{/snippet}

<div class="intro">
  {#if unconfigured}
    <p class="lead-text">{s.unconfigured}</p>
    <p class="lead-text"><a class="linkbtn" href={href({ view: "self" })}>{s.unconfiguredLink}</a></p>
  {:else}
    {#if configFailed}
      <!-- Without /config nothing can be changed; the values below come from /status. -->
      <LoadError text={t.load.configFailed} onretry={() => app.loadConfig()} />
    {:else}
      <p class="lead-text">{c && !canEdit ? s.introReadOnly : s.intro}</p>
      {#if app.offline && canEdit}<p class="hint bad">{s.offline}</p>{/if}
    {/if}
  {/if}
</div>

{#if !unconfigured}
  <div class="prefs">
    <section class="pgrp" aria-labelledby="pg-schedule">
      <h2 id="pg-schedule">{s.schedule}</h2>
      <div class="box">
        <div class="pref" class:err={!!problems.automatic}>
          <div class="txt">
            <span class="l" id="l-auto">{s.automatic}</span>
            <p class="h" id="h-auto">{(optimistic.automatic ?? automatic) ? s.automaticOn : s.automaticOff}</p>
          </div>
          <div class="c">
            {@render savedMark("automatic")}
            {#if editable.has("automatic_backups")}
              <Switch
                checked={optimistic.automatic ?? automatic}
                labelledby="l-auto"
                describedby={describe("h-auto", problems.automatic && "e-auto")}
                disabled={!can("automatic_backups") || !!pending.automatic}
                busy={!!pending.automatic}
                onchange={setAutomatic}
              />
            {:else}
              <span class="ro">{automatic ? s.on : s.off}</span>
            {/if}
          </div>
          {@render problemLine("automatic", "e-auto")}
        </div>

        <div class="pref" class:err={!!problems.interval}>
          <div class="txt">
            {#if editable.has("interval_hours")}
              <label class="l" for="interval">{s.interval}</label>
            {:else}
              <span class="l">{s.interval}</span>
            {/if}
            <p class="h" id="h-interval">{s.intervalHint}</p>
          </div>
          <div class="c">
            {@render savedMark("interval")}
            {#if editable.has("interval_hours") && interval != null}
              <select
                id="interval"
                value={String(interval)}
                disabled={!can("interval_hours") || !!pending.interval}
                aria-describedby={describe("h-interval", problems.interval && "e-interval")}
                onchange={changeInterval}
              >
                {#each intervalOptions(interval) as h (h)}
                  <option value={String(h)}>{s.everyHours(h)}</option>
                {/each}
              </select>
            {:else}
              <span class="ro">{interval != null ? s.everyHours(interval) : (c?.schedule ?? status.schedule.calendar ?? "–")}</span>
            {/if}
          </div>
          {@render problemLine("interval", "e-interval")}
        </div>

        <div class="pref" class:err={!!problems.overdue}>
          <div class="txt">
            {#if editable.has("overdue_after_hours")}
              <label class="l" for="overdue">{s.overdue}</label>
              <p class="h" id="h-overdue">{raised ? s.overdueRaised(raised) : s.overdueHint}</p>
            {:else}
              <span class="l">{s.overdue}</span>
            {/if}
          </div>
          <div class="c">
            {@render savedMark("overdue")}
            {#if editable.has("overdue_after_hours")}
              <input
                class="inp n"
                id="overdue"
                inputmode="numeric"
                autocomplete="off"
                bind:value={overdueRaw}
                disabled={!can("overdue_after_hours") || !!pending.overdue}
                aria-invalid={problems.overdue ? "true" : undefined}
                aria-describedby={describe("h-overdue", problems.overdue && "e-overdue")}
                onchange={commitOverdue}
                onkeydown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    void commitOverdue();
                  }
                }}
              />
              <span class="unit">{s.hours}</span>
            {:else}
              <span class="ro">{overdue != null ? `${overdue} ${s.hours}` : "–"}</span>
            {/if}
          </div>
          {@render problemLine("overdue", "e-overdue")}
        </div>
      </div>
    </section>

    <section class="pgrp" aria-labelledby="pg-excludes">
      <h2 id="pg-excludes">{s.excludes}</h2>
      <p class="sub">{sources.length ? s.excludesHint(sources.join(", ")) : s.excludesHintPlain}</p>
      <div class="box">
        {#if c}
          {#each excludes as p (p)}
            <div class="pref sw">
              <code class="pat">{p}</code>
              {#if editable.has("excludes")}
                <div class="c">
                  <button
                    class="btn small"
                    type="button"
                    aria-label={s.removeLabel(p)}
                    disabled={!can("excludes") || !!pending.excludes}
                    onclick={() => removePattern(p)}>{s.remove}</button
                  >
                </div>
              {/if}
            </div>
          {:else}
            <div class="pref"><span class="muted">{s.excludesNone}</span></div>
          {/each}
        {:else}
          <div class="pref"><span class="muted">{configFailed ? t.load.notLoaded : t.load.loading}</span></div>
        {/if}
        {#if editable.has("excludes")}
          <div class="pref" class:err={!!problems.excludes}>
            <form class="addrow" onsubmit={addPattern}>
              <label class="sr" for="pattern">{s.newPattern}</label>
              <input
                class="inp mono"
                id="pattern"
                autocomplete="off"
                spellcheck="false"
                placeholder={s.placeholder}
                bind:value={pattern}
                bind:this={patternInput}
                disabled={!can("excludes")}
                aria-invalid={problems.excludes ? "true" : undefined}
                aria-describedby={problems.excludes ? "e-excludes" : undefined}
                oninput={() => (problems.excludes = undefined)}
              />
              <button class="btn" type="submit" disabled={!can("excludes") || !!pending.excludes}>{s.add}</button>
            </form>
            {#if problems.excludes}
              {@render problemLine("excludes", "e-excludes")}
            {:else if pending.excludes || saved.excludes}
              <p class="e ok">{@render savedMark("excludes")}</p>
            {/if}
          </div>
        {/if}
      </div>
    </section>

    <section class="pgrp" aria-labelledby="pg-wake">
      <h2 id="pg-wake">{s.wake}</h2>
      <div class="box">
        <div class="pref" class:err={!!problems.wol}>
          <div class="txt">
            <span class="l" id="l-wol">{s.wol}</span>
            <p class="h" id="h-wol">{wolConfigured === false ? s.wolNotConfigured : s.wolHint}</p>
          </div>
          <div class="c">
            {@render savedMark("wol")}
            {#if editable.has("wake_on_lan")}
              <Switch
                checked={optimistic.wol ?? wol}
                labelledby="l-wol"
                describedby={describe("h-wol", problems.wol && "e-wol")}
                disabled={!can("wake_on_lan") || !!pending.wol}
                busy={!!pending.wol}
                onchange={setWol}
              />
            {:else}
              <span class="ro">{wol ? s.on : s.off}</span>
            {/if}
          </div>
          {@render problemLine("wol", "e-wol")}
        </div>
      </div>
    </section>

    <section class="pgrp" aria-labelledby="pg-fixed">
      <h2 id="pg-fixed">{s.fixed}</h2>
      <p class="sub">{s.fixedHint}</p>
      <dl class="box">
        <div class="pref ro-row"><dt class="l">{s.address}</dt><dd class="c">{status.pbs.server}</dd></div>
        <div class="pref ro-row"><dt class="l">{t.server.datastore}</dt><dd class="c">{status.pbs.datastore}</dd></div>
        <div class="pref ro-row">
          <dt class="l">{t.server.namespace}</dt><dd class="c">{status.pbs.namespace || t.server.noNamespace}</dd>
        </div>
        <div class="pref ro-row">
          <dt class="l">{s.backupId}</dt><dd class="c"><code>{c?.backup_id ?? status.machine.backup_id}</code></dd>
        </div>
        <div class="pref ro-row">
          <dt class="l">{s.encryption}</dt>
          <dd class="c">
            {!status.encryption.enabled
              ? t.self.encryptionOff
              : status.encryption.key_exported
                ? t.self.encryptionOnKey
                : t.self.encryptionOnNoKey}
          </dd>
        </div>
        {#if status.encryption.key_fingerprint}
          <div class="pref ro-row">
            <dt class="l">{s.fingerprint}</dt><dd class="c"><code>{status.encryption.key_fingerprint}</code></dd>
          </div>
        {/if}
        <div class="pref ro-row"><dt class="l">{s.retention}</dt><dd class="c">{s.retentionText}</dd></div>
      </dl>
      {#if status.encryption.enabled && status.encryption.key_exported}
        <p class="hint">{s.keyVerify}</p>
      {/if}
      <Cmd label={s.changeIn} command="sudoedit /etc/nestegg/config.toml" />
      <Cmd label={s.checkIn} command="sudo nestegg check" />
    </section>
  </div>
{/if}

<style>
  .intro { padding-bottom: 18px; border-bottom: 1px solid var(--rule); margin-bottom: 26px; }
  .prefs { max-width: 720px; }
  .pgrp { margin: 0 0 30px; }
  .pgrp > h2 { margin: 0 0 8px; font-size: var(--fs-m); font-weight: 600; }
  .sub { color: var(--muted); margin: -4px 0 10px; font-size: 13px; }
  .box { margin: 0; }
  .pref {
    display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 4px 20px; align-items: center;
    padding: 12px 14px; border-top: 1px solid var(--rule);
  }
  .pref:first-child { border-top: 0; border-radius: 8px 8px 0 0; }
  .pref:last-child { border-radius: 0 0 8px 8px; }
  .pref:only-child { border-radius: 8px; }
  .txt { min-width: 0; }
  .l { font-weight: 500; display: block; }
  .h { color: var(--muted); font-size: 13px; margin: 0; }
  .c { display: flex; align-items: center; gap: 10px; justify-self: end; min-width: 0; }
  .ro { color: var(--muted); }
  .pref.err { background: var(--err-bg); }
  .e { grid-column: 1 / -1; color: var(--bad); font-size: 13px; margin: 2px 0 0; }
  .e.ok { color: inherit; }
  .detail { display: block; color: var(--muted); overflow-wrap: anywhere; }
  .saved, .saving { font-size: var(--fs-s); white-space: nowrap; }
  .saving { color: var(--muted); }
  .saved { color: var(--ok); display: inline-flex; align-items: center; gap: 6px; }
  .saved::before {
    content: ""; width: 9px; height: 5px; border-left: 2px solid currentColor; border-bottom: 2px solid currentColor;
    transform: rotate(-45deg) translateY(-2px);
  }
  select, .inp {
    background: var(--field); border: 1px solid var(--field-border); border-radius: 6px; padding: 6px 10px; min-height: 34px;
  }
  select { padding-right: 28px; max-width: 100%; }
  select:disabled, .inp:disabled { opacity: 0.6; }
  .inp.n { width: 72px; text-align: right; }
  .unit { color: var(--muted); }
  .pref.err .inp { border-color: var(--bad); box-shadow: inset 0 0 0 1px var(--bad); }
  .pat { font-size: 13.5px; overflow-wrap: anywhere; min-width: 0; }
  .addrow { grid-column: 1 / -1; display: flex; gap: 8px; min-width: 0; }
  .addrow .inp { flex: 1; min-width: 0; font-size: 13.5px; }
  .ro-row dt { font-weight: 500; }
  .ro-row dd { color: var(--muted); text-align: right; overflow-wrap: anywhere; }
  .pgrp :global(.cmd) { margin-top: 8px; }

  @media (max-width: 760px) {
    .pref { grid-template-columns: minmax(0, 1fr); }
    .c { justify-self: start; flex-wrap: wrap; }
    /* Rows with a switch or a remove button keep the control on the right. */
    .pref:has(:global(.switch)), .pref.sw { grid-template-columns: minmax(0, 1fr) auto; }
    .pref:has(:global(.switch)) .c, .pref.sw .c { justify-self: end; }
    .ro-row dd { text-align: left; }
    select { min-height: 40px; }
    .inp { min-height: 40px; }
  }
</style>
