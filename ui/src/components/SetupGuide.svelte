<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import { initCommand, type Guide, type GuideStep } from "../lib/derive";
  import { t } from "../lib/locale";

  /**
   * The way from a fresh install to the first backup. Everything happens in a
   * terminal as root; this page never asks for a password, token secret or key.
   * It shows where the user is and moves on by itself when /status says so.
   */
  let { guide }: { guide: Guide } = $props();

  const g = t.guide;
  const titles: Record<GuideStep, string> = {
    setup: g.setup,
    key_create: g.keyCreate,
    key_store: g.keyStore,
    first_backup: g.firstBackup,
  };
  const at = $derived(guide.steps.indexOf(guide.current));

  // Not secret, but real names of the user's server: kept in memory only.
  let server = $state("");
  let datastore = $state("");
  let namespace = $state("");
  let authId = $state("");
  const initCmd = $derived(initCommand({ server, datastore, namespace, authId }));
</script>

<section class="guide" aria-labelledby="h-guide">
  <h2 class="sec-title" id="h-guide">{g.title}</h2>
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
            {#if step === "setup"}
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
              <ol class="cmds">
                <li><Cmd command={initCmd} /><p class="hint">{g.cmdInit}</p></li>
                <li><Cmd command="sudo nestegg credentials set-password" /><p class="hint">{g.cmdPassword}</p></li>
                <li><Cmd command="sudo nestegg check" /><p class="hint">{g.cmdCheck}</p></li>
                <li><Cmd command="sudo systemctl enable --now nestegg.service nestegg-backup.timer" /><p class="hint">{g.cmdEnable}</p></li>
                <li><Cmd command={'sudo usermod -aG nestegg "$USER"'} /><p class="hint">{g.cmdGroup}</p></li>
              </ol>
              <p class="hint">{g.fingerprint}</p>
            {:else if step === "key_create"}
              <p class="text">{g.keyCreateWhy}</p>
              <Cmd command="sudo nestegg key create" />
              <p class="hint">{g.keyCreateImport}</p>
            {:else if step === "key_store"}
              <p class="text">{g.keyStoreWhy}</p>
              <div class="ways">
                <section aria-labelledby="h-usb">
                  <h3 id="h-usb">{g.usbTitle}</h3>
                  <p class="text">{g.usbText}</p>
                  <Cmd command="sudo nestegg key export /media/<stick>/nestegg-key.json" />
                  <p class="hint">{g.usbConfirm}</p>
                  <Cmd command="sudo nestegg key confirm-exported --file /media/<stick>/nestegg-key.json" />
                </section>
                <section aria-labelledby="h-paper">
                  <h3 id="h-paper">{g.paperTitle}</h3>
                  <p class="text">{g.paperText}</p>
                  <Cmd command="sudo nestegg key paperkey --output ~/nestegg-paper-key.html" />
                  <p class="hint">{g.paperConfirm}</p>
                  <Cmd command="sudo nestegg key confirm-exported --paper-code <CODE>" />
                </section>
              </div>
            {:else}
              <p class="text">{g.firstBackupText}</p>
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
  .cmds li { margin-top: 12px; }
  .cmds .hint { margin-top: 4px; }
  .ways { display: grid; gap: 22px; margin-top: 14px; }
  .ways section { border-left: 2px solid var(--rule); padding-left: 14px; }
  h3 { font-size: var(--fs-m); font-weight: 600; }
  @media (max-width: 760px) {
    .fields { grid-template-columns: minmax(0, 1fr); }
  }
</style>
