<script lang="ts">
  import { tick } from "svelte";
  import Cmd from "./Cmd.svelte";
  import { reveal } from "../lib/reveal";
  import {
    CHECK_COMMAND,
    ENABLE_TIMERS_COMMAND,
    KEY_CREATE_COMMAND,
    PAPER_KEY_COMMAND,
    PAPER_KEY_DELETE_COMMAND,
    SET_PASSWORD_COMMAND,
    initCommand,
    keyConfirmCodeCommand,
    keyConfirmFileCommand,
    keyExportCommand,
    normalizeCheckCode,
    STICK_INFO_COMMAND,
    isUuid,
    knownStickUuid,
    stickFile,
    stickKeyCopyCommand,
    stickOnly,
    type Guide,
    type GuideStep,
  } from "../lib/derive";
  import { ApiFailure } from "../lib/api";
  import { saveFailure } from "../lib/settingsForm";
  import { app } from "../lib/store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";
  import type { Status } from "../lib/types";

  /**
   * The way from a fresh install to the first backup. Everything happens in a
   * terminal as root; this page never asks for a password, token secret or key.
   * It shows where the user is and moves on by itself when /status says so.
   * A command that still has a placeholder cannot be copied (see Cmd).
   */
  let { guide, status }: { guide: Guide; status: Status } = $props();

  // Stick only: the password-protected copy on the backup stick counts as the key proof (decision 0026).
  const onStick = $derived(stickOnly(status));
  const knownUuid = $derived(onStick ? knownStickUuid(app.runs.value, status.targets?.stick, app.config.value?.stick_data_uuids) : null);
  let uuid = $state("");
  const stickUuid = $derived(knownUuid ?? uuid);
  const uuidBad = $derived(!knownUuid && uuid.trim() !== "" && !isUuid(uuid));

  const g = t.guide;
  const titles: Record<GuideStep, string> = {
    connect: g.connect,
    credentials: g.credentials,
    key_create: g.keyCreate,
    key_store: g.keyStore,
    automatic: g.automatic,
    first_backup: g.firstBackup,
  };
  const at = $derived(guide.steps.indexOf(guide.current));

  // Not secret, but real names of the user's server: kept in memory only.
  let server = $state("");
  let datastore = $state("");
  let namespace = $state("");
  let authId = $state("");
  let stick = $state("");
  let code = $state("");
  const initCmd = $derived(initCommand({ server, datastore, namespace, authId }));
  const stickBad = $derived(stick.trim() !== "" && !stickFile(stick));
  const codeBad = $derived(code.trim() !== "" && !normalizeCheckCode(code));

  // "Switch on automatic backups" is the same change as the switch in the settings.
  const canSwitch = $derived(!!app.config.value?.editable?.includes("automatic_backups"));
  let switching = $state(false);
  let switchProblem = $state<string | null>(null);
  let heading: HTMLElement | undefined = $state();
  let problemEl: HTMLElement | undefined = $state();

  async function switchOn() {
    // aria-disabled instead of disabled: the button keeps the focus while it works.
    if (switching || app.offline) return;
    switching = true;
    switchProblem = null;
    try {
      await app.saveSettings({ automatic_backups: true });
      statusBar.show(t.sbar.saved(f.time(new Date()), t.settings.sumAutomatic(true)));
      // The step is done and the button goes away with it: the focus moves to the list of steps.
      await tick();
      heading?.focus();
    } catch (e) {
      const failure = e instanceof ApiFailure ? e : new ApiFailure("other", 0, null, String(e));
      switchProblem = saveFailure(failure, t).text;
      await tick();
      problemEl?.focus();
    } finally {
      switching = false;
    }
  }
</script>

<section class="guide" aria-labelledby="h-guide">
  <h2 class="sec-title" id="h-guide" tabindex="-1" bind:this={heading}>{g.title}</h2>
  <ol class="steps" aria-label={g.stepsLabel}>
    {#each guide.steps as step, i (step)}
      <li class="step" class:done={i < at} class:now={i === at} aria-current={i === at ? "step" : undefined}>
        <span class="n" aria-hidden="true">{i + 1}</span>
        <div class="body">
          <p class="title">
            {titles[step]}
            <span class="sr">, {i < at ? g.done : i === at ? g.now : g.later}</span>
          </p>
          {#if i === at}
            {#if step === "connect"}
              <p class="text">{g.setupIntro}</p>
              <p class="text">{g.setupClient}</p>
              <div class="fields">
                <div class="field">
                  <label for="g-server">{g.server}</label>
                  <input id="g-server" class="input" autocomplete="off" spellcheck="false" placeholder={g.serverPlaceholder} bind:value={server} />
                </div>
                <div class="field">
                  <label for="g-ds">{g.datastore}</label>
                  <input id="g-ds" class="input" autocomplete="off" spellcheck="false" placeholder={g.datastorePlaceholder} bind:value={datastore} />
                </div>
                <div class="field">
                  <label for="g-ns">{g.namespace}</label>
                  <input id="g-ns" class="input" autocomplete="off" spellcheck="false" placeholder={g.namespacePlaceholder} bind:value={namespace} />
                </div>
                <div class="field">
                  <label for="g-auth">{g.authId}</label>
                  <input id="g-auth" class="input" autocomplete="off" spellcheck="false" placeholder={g.authIdPlaceholder} bind:value={authId} aria-describedby="g-auth-hint" />
                  <p class="hint" id="g-auth-hint">{g.authIdHint}</p>
                </div>
              </div>
              <p class="hint">{g.fieldsNote}</p>
              <p class="text lead-in">{g.commandsIntro}</p>
              <Cmd command={initCmd} missing={g.fillFields} />
              <p class="hint">{g.cmdInit}</p>
              <p class="hint">{g.fingerprint}</p>
              <p class="hint">{g.groupNote}</p>
            {:else if step === "credentials"}
              <p class="text">{g.credentialsWhy}</p>
              <ol class="cmds">
                <li><Cmd command={SET_PASSWORD_COMMAND} /><p class="hint">{g.cmdPassword}</p></li>
                <li><Cmd command={CHECK_COMMAND} /><p class="hint">{g.cmdCheck}</p><p class="hint">{g.cmdCheckNote}</p></li>
              </ol>
            {:else if step === "key_create"}
              <p class="text">{g.keyCreateWhy}</p>
              <Cmd command={KEY_CREATE_COMMAND} />
              <p class="hint">{g.keyCreateImport}</p>
            {:else if step === "key_store"}
              <p class="text">{g.keyStoreWhy}</p>
              <div class="ways">
                {#if onStick}
                  <section aria-labelledby="h-stickcopy">
                    <h3 id="h-stickcopy">{g.stickCopyTitle}</h3>
                    <p class="text">{g.stickCopyText}</p>
                    {#if !knownUuid}
                      <div class="field one">
                        <label for="g-uuid">{t.stick.uuidLabel}</label>
                        <input
                          id="g-uuid" class="input" autocomplete="off" spellcheck="false" placeholder="01234567-89ab-cdef-0123-456789abcdef"
                          bind:value={uuid} aria-invalid={uuidBad ? "true" : undefined}
                          aria-describedby={uuidBad ? "g-uuid-hint g-uuid-bad" : "g-uuid-hint"}
                        />
                        <p class="hint" id="g-uuid-hint">{t.stick.uuidHint}</p>
                        {#if uuidBad}<p class="hint bad" id="g-uuid-bad" use:reveal>{t.stick.uuidInvalid}</p>{/if}
                      </div>
                      <Cmd command={STICK_INFO_COMMAND} />
                    {/if}
                    <Cmd command={stickKeyCopyCommand(stickUuid)} missing={t.stick.fillUuid} />
                    <p class="hint">{g.stickCopyPaper}</p>
                  </section>
                {/if}
                <section aria-labelledby="h-usb">
                  <h3 id="h-usb">{g.usbTitle}</h3>
                  <p class="text">{g.usbText}</p>
                  <div class="field one">
                    <label for="g-stick">{g.stickLabel}</label>
                    <input
                      id="g-stick" class="input" autocomplete="off" spellcheck="false" placeholder={g.stickPlaceholder}
                      bind:value={stick} aria-invalid={stickBad ? "true" : undefined}
                      aria-describedby={stickBad ? "g-stick-hint g-stick-bad" : "g-stick-hint"}
                    />
                    <p class="hint" id="g-stick-hint">{g.stickHint}</p>
                    {#if stickBad}<p class="hint bad" id="g-stick-bad" use:reveal>{g.stickInvalid}</p>{/if}
                  </div>
                  <Cmd command={keyExportCommand(stick)} missing={g.fillStick} />
                  <p class="hint">{g.usbConfirm}</p>
                  <Cmd command={keyConfirmFileCommand(stick)} missing={g.fillStick} />
                  <p class="hint">{g.usbUnplug}</p>
                  <p class="hint">{g.usbLocal}</p>
                </section>
                <section aria-labelledby="h-paper">
                  <h3 id="h-paper">{g.paperTitle}</h3>
                  <p class="text">{g.paperText}</p>
                  <ol class="paper">
                    <li>
                      <p class="text">{g.paperWrite}</p>
                      <Cmd command={PAPER_KEY_COMMAND} />
                    </li>
                    <li><p class="text">{g.paperPrint}</p></li>
                    <li>
                      <p class="text">{g.paperDelete}</p>
                      <Cmd command={PAPER_KEY_DELETE_COMMAND} />
                    </li>
                    <li>
                      <p class="text">{g.paperConfirm}</p>
                      <div class="field one">
                        <label for="g-code">{g.codeLabel}</label>
                        <input
                          id="g-code" class="input code" autocomplete="off" spellcheck="false" placeholder={g.codePlaceholder}
                          bind:value={code} aria-invalid={codeBad ? "true" : undefined}
                          aria-describedby={codeBad ? "g-code-hint g-code-bad" : "g-code-hint"}
                        />
                        <p class="hint" id="g-code-hint">{g.codeHint}</p>
                        {#if codeBad}<p class="hint bad" id="g-code-bad" use:reveal>{g.codeInvalid}</p>{/if}
                      </div>
                      <Cmd command={keyConfirmCodeCommand(code)} missing={g.fillCode} />
                      <p class="hint">{g.paperAsk}</p>
                    </li>
                  </ol>
                </section>
              </div>
            {:else if step === "automatic"}
              <p class="text">{g.automaticWhy}</p>
              {#if canSwitch}
                <div class="actions">
                  <button
                    class="btn primary"
                    type="button"
                    aria-disabled={switching || app.offline ? "true" : undefined}
                    onclick={switchOn}>{switching ? g.automaticWorking : g.automaticButton}</button
                  >
                </div>
                {#if switchProblem}
                  <p class="hint bad" tabindex="-1" bind:this={problemEl}>{g.automaticFailed(switchProblem)}</p>
                {/if}
                <p class="hint or">{g.automaticOr}</p>
              {/if}
              <Cmd command={ENABLE_TIMERS_COMMAND} />
            {:else}
              <p class="text">{onStick ? g.firstBackupStick : g.firstBackupText}</p>
            {/if}
            <p class="hint foot">{step === "first_backup" ? g.moveOn : `${g.terminal} ${g.moveOn}`}</p>
          {/if}
        </div>
      </li>
    {/each}
  </ol>
</section>

<style>
  .guide { margin-top: 26px; max-width: 720px; }
  .steps { display: grid; gap: 14px; margin-top: 12px; }
  .step { display: grid; grid-template-columns: 22px minmax(0, 1fr); gap: 12px; align-items: start; color: var(--faint); }
  .n {
    width: 22px; height: 22px; border-radius: 50%; display: grid; place-items: center;
    font-size: var(--fs-s); box-shadow: inset 0 0 0 1px var(--field-border);
  }
  .step.now { color: var(--text); }
  .step.now .n { box-shadow: inset 0 0 0 2px var(--accent-text); }
  .step.done { color: var(--muted); }
  .step.done .n { background: var(--tick); box-shadow: none; color: var(--on-tick); }
  .title { font-weight: 500; line-height: 22px; }
  .text { color: var(--muted); margin-top: 6px; max-width: 66ch; }
  .lead-in { margin-top: 18px; color: var(--text); }
  .hint { max-width: 66ch; }
  .foot { margin-top: 16px; }
  .fields { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px 16px; margin-top: 16px; max-width: 640px; }
  .field label { display: block; font-weight: 500; font-size: 13px; margin-bottom: 4px; }
  .field .hint { margin-top: 4px; font-size: var(--fs-s); }
  .input {
    width: 100%; background: var(--field); border: 1px solid var(--field-border); border-radius: 6px;
    padding: 7px 10px; min-height: 36px; font-family: var(--mono); font-size: 13.5px;
  }
  .input:focus-visible { outline: 2px solid var(--accent-text); outline-offset: 1px; }
  .cmds { margin-top: 2px; }
  .actions { margin-top: 12px; }
  .or { margin-top: 12px; }
  .field.one { margin-top: 12px; max-width: 420px; }
  .input[aria-invalid="true"] { border-color: var(--bad); }
  .cmds li { margin-top: 12px; }
  .paper { list-style: decimal; padding-left: 20px; margin-top: 4px; }
  .paper li { margin-top: 12px; padding-left: 4px; }
  .paper li::marker { color: var(--muted); }
  .cmds .hint { margin-top: 4px; }
  .ways { display: grid; gap: 22px; margin-top: 14px; }
  .ways section { border-left: 2px solid var(--rule); padding-left: 14px; }
  h3 { font-size: var(--fs-m); font-weight: 600; }
  @media (max-width: 760px) {
    .fields { grid-template-columns: minmax(0, 1fr); }
  }
</style>
