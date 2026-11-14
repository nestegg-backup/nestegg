<script lang="ts">
  import { tick } from "svelte";
  import Cmd from "../components/Cmd.svelte";
  import Dot from "../components/Dot.svelte";
  import { asHubFailure } from "./api";
  import { hubName, MASTER_KEY_CONFIRM_COMMAND, minutesLeft, newMachines, packageCommand, validityText } from "./derive";
  import { hub } from "./store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";
  import type { HubInfo, JoinCode } from "./types";

  /**
   * "Add computer": one command line for the new computer. The hub refuses while the master key
   * copy is not proven or it has no package to hand out; both are shown as a state with the
   * command for the hub's terminal, not as an error. While the dialog is open the list is asked
   * every few seconds, so the new computer appears by itself and the dialog says so.
   */
  let { open = $bindable(false) }: { open: boolean } = $props();

  const d = t.hub.dialog;
  type View =
    | { kind: "form" }
    | { kind: "creating" }
    | { kind: "master" }
    | { kind: "package" }
    | { kind: "code"; code: JoinCode; before: Set<string> }
    | { kind: "joined"; name: string }
    | { kind: "revoked" };

  let view = $state<View>({ kind: "form" });
  let note = $state("");
  let problem = $state<string | null>(null);
  let checking = $state(false);
  let revoking = $state(false);
  let dlg: HTMLDialogElement | undefined = $state();
  let body: HTMLElement | undefined = $state();
  let fast = false;

  /** What the hub's state says before asking for a code; the hub's answer to the request decides. */
  function preflight(h: HubInfo | null): View {
    if (h && (!h.master_key.present || !h.master_key.proven)) return { kind: "master" };
    if (h && !h.package.available) return { kind: "package" };
    return { kind: "form" };
  }

  $effect(() => {
    if (!dlg) return;
    if (open && !dlg.open) {
      problem = null;
      note = "";
      view = preflight(hub.hub);
      dlg.showModal();
      if (!fast) {
        fast = true;
        hub.setFast(true);
      }
      // The hub's state may have changed in its terminal since the page loaded.
      void hub.loadHub().then((h) => {
        if (view.kind === "form" || view.kind === "master" || view.kind === "package") view = preflight(h);
      });
    } else if (!open && dlg.open) {
      dlg.close();
    }
  });

  function onClose() {
    open = false;
    if (fast) {
      fast = false;
      hub.setFast(false);
    }
  }

  // The new computer shows up in the list: the dialog says so.
  $effect(() => {
    if (view.kind !== "code" || !hub.machines) return;
    const fresh = newMachines(view.before, hub.machines.machines);
    if (fresh.length) {
      const name = hubName(fresh[0]!);
      view = { kind: "joined", name };
      statusBar.show(d.joined(name));
    }
  });

  const expired = $derived(view.kind === "code" && minutesLeft(view.code.expires_at, hub.now) === 0);

  async function focusFirst(selector: string) {
    await tick();
    body?.querySelector<HTMLElement>(selector)?.focus();
  }

  async function create() {
    if (view.kind === "creating") return;
    // Known before the request: a computer that joins very quickly still counts as new.
    const before = new Set((hub.machines?.machines ?? []).map((m) => m.machine_id));
    problem = null;
    view = { kind: "creating" };
    try {
      const code = await hub.createCode(note);
      view = { kind: "code", code, before };
      await focusFirst(".cmd button");
    } catch (err) {
      const fail = asHubFailure(err);
      if (fail.unauthorized) return;
      if (fail.code === "master_key_unproven") {
        view = { kind: "master" };
        void hub.loadHub();
      } else if (fail.code === "no_package") {
        view = { kind: "package" };
        void hub.loadHub();
      } else {
        view = { kind: "form" };
        problem = fail.status === 0 ? d.offline : d.failed(fail.message);
      }
    }
  }

  async function recheck() {
    if (checking) return;
    checking = true;
    const h = await hub.loadHub();
    checking = false;
    view = preflight(h);
    if (view.kind === "form") await focusFirst("input");
  }

  async function revoke() {
    if (view.kind !== "code" || revoking) return;
    revoking = true;
    problem = null;
    try {
      await hub.revoke(view.code.code_id);
      view = { kind: "revoked" };
      await focusFirst("p.text");
    } catch (err) {
      const fail = asHubFailure(err);
      if (!fail.unauthorized) problem = t.hub.codes.revokeFailed(fail.message);
    } finally {
      revoking = false;
    }
  }

  async function again() {
    problem = null;
    note = "";
    view = preflight(hub.hub);
    if (view.kind === "form") await focusFirst("input");
  }
</script>

<dialog class="dlg" bind:this={dlg} onclose={onClose} aria-labelledby="add-title">
  <form class="frame" onsubmit={(e) => { e.preventDefault(); if (view.kind === "form") void create(); }}>
    <h2 class="dtitle" id="add-title">{d.title}</h2>
    <div class="dbody" bind:this={body}>
      {#if view.kind === "form" || view.kind === "creating"}
        <p class="text">{d.intro}</p>
        <div class="field">
          <label for="add-note">{d.note}</label>
          <input
            id="add-note" class="input" maxlength="80" autocomplete="off" placeholder={d.notePlaceholder}
            bind:value={note} aria-describedby="add-note-hint"
          />
          <p class="hint" id="add-note-hint">{d.noteHint}</p>
        </div>
      {:else if view.kind === "master" || view.kind === "package"}
        <!-- A refusal of the hub is a state with an instruction, not an error. -->
        <h3 class="state"><Dot tone="warn" />{view.kind === "master" ? d.masterTitle : d.packageTitle}</h3>
        <p class="text">{view.kind === "master" ? d.masterText : d.packageText}</p>
        {#if view.kind === "master"}
          <Cmd command={MASTER_KEY_CONFIRM_COMMAND} />
        {:else}
          <Cmd command={packageCommand(t)} missing={d.packageMissing} />
        {/if}
        <p class="hint">{d.terminal}</p>
      {:else if view.kind === "code"}
        <h3 class="sub-title">{d.howTitle}</h3>
        <ol class="how">
          <li>
            <p>{d.how1}</p>
            <Cmd command={view.code.command} keep={view.code.code} />
          </li>
          <li><p>{d.how2}</p></li>
          <li><p>{d.how3(f.time(view.code.expires_at))}</p></li>
        </ol>
        <p class="valid" class:expired>
          {#if expired}<Dot tone="warn" />{/if}{validityText(view.code.expires_at, t, f, hub.now)}
        </p>
        {#if expired}
          <p class="hint">{d.expiredText}</p>
        {:else}
          <p class="hint">{d.once}</p>
          <p class="hint wait"><span class="pulse" aria-hidden="true"></span>{d.waiting}</p>
        {/if}
      {:else if view.kind === "joined"}
        <h3 class="state"><Dot tone="ok" />{d.joined(view.name)}</h3>
        <p class="text">{d.joinedText}</p>
      {:else if view.kind === "revoked"}
        <p class="text" tabindex="-1">{d.revoked}</p>
      {/if}
      <p class="hint bad" role="alert">{problem ?? ""}</p>
    </div>

    <!-- The main button comes last, on the right. -->
    <div class="dfoot">
      {#if view.kind === "form" || view.kind === "creating"}
        <button class="btn" type="button" onclick={() => (open = false)}>{d.cancel}</button>
        <button class="btn primary" type="submit" aria-disabled={view.kind === "creating" ? "true" : undefined}>
          {view.kind === "creating" ? d.creating : d.create}
        </button>
      {:else if view.kind === "master" || view.kind === "package"}
        <button class="btn" type="button" onclick={() => (open = false)}>{d.close}</button>
        <button class="btn primary" type="button" aria-disabled={checking ? "true" : undefined} onclick={recheck}>
          {checking ? d.checking : d.checkAgain}
        </button>
      {:else if view.kind === "code"}
        {#if expired}
          <button class="btn" type="button" onclick={() => (open = false)}>{d.close}</button>
          <button class="btn primary" type="button" onclick={again}>{d.again}</button>
        {:else}
          <button class="btn" type="button" aria-disabled={revoking ? "true" : undefined} onclick={revoke}>{d.revoke}</button>
          <button class="btn" type="button" onclick={() => (open = false)}>{d.close}</button>
        {/if}
      {:else}
        <button class="btn" type="button" onclick={again}>{view.kind === "joined" ? d.another : d.again}</button>
        <button class="btn primary" type="button" onclick={() => (open = false)}>{d.close}</button>
      {/if}
    </div>
  </form>
</dialog>

<style>
  .dlg {
    width: min(680px, calc(100vw - 32px)); max-height: calc(100vh - 48px); padding: 0;
    background: var(--win); color: var(--text); border: 1px solid var(--rule); border-radius: 10px;
    box-shadow: 0 18px 48px rgb(0 0 0 / 0.35);
  }
  .dlg::backdrop { background: rgb(0 0 0 / 0.5); }
  .frame { display: flex; flex-direction: column; max-height: calc(100vh - 48px); }
  .dtitle { font-size: var(--fs-t); font-weight: 600; padding: 16px 20px 0; }
  .dbody { padding: 10px 20px 4px; overflow-y: auto; min-height: 0; }
  .dfoot {
    display: flex; justify-content: flex-end; gap: 8px; flex-wrap: wrap;
    padding: 14px 20px 16px; border-top: 1px solid var(--rule); margin-top: 12px;
  }
  .text { color: var(--muted); max-width: 66ch; }
  .text:focus { outline: none; }
  .field { margin-top: 16px; max-width: 420px; }
  .field label { display: block; font-weight: 500; font-size: 13px; margin-bottom: 4px; }
  .field .hint { margin-top: 4px; font-size: var(--fs-s); }
  .input {
    width: 100%; background: var(--field); border: 1px solid var(--field-border); border-radius: 6px;
    padding: 7px 10px; min-height: 36px;
  }
  .state { font-size: var(--fs-m); font-weight: 600; display: flex; align-items: center; gap: 10px; margin: 4px 0 6px; }
  .sub-title { font-size: 13px; font-weight: 600; color: var(--muted); margin: 4px 0 6px; }
  .how { list-style: decimal; padding-left: 20px; }
  .how li { margin-top: 10px; padding-left: 4px; }
  .how li::marker { color: var(--muted); }
  .how li p { max-width: 66ch; }
  .how :global(.cmd) { max-width: none; }
  .valid { margin-top: 16px; font-weight: 600; display: flex; align-items: center; gap: 10px; }
  .valid.expired { color: var(--warn); }
  .wait { display: flex; align-items: baseline; gap: 8px; }
  .pulse {
    flex: none; width: 7px; height: 7px; border-radius: 50%; background: var(--accent-text);
    animation: pulse 1.6s ease-in-out infinite; transform: translateY(-1px);
  }
  @keyframes pulse { 50% { opacity: 0.3; } }
  .bad:empty { margin: 0; }

  @media (max-width: 760px) {
    .dlg { width: calc(100vw - 16px); max-height: calc(100vh - 16px); }
    .frame { max-height: calc(100vh - 16px); }
    .dtitle { padding: 14px 16px 0; }
    .dbody { padding: 8px 16px 4px; }
    .dfoot { padding: 12px 16px 14px; }
    .dfoot .btn { flex: 1 1 auto; padding: 10px 12px; }
  }
</style>
