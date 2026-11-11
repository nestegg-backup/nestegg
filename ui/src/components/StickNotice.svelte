<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { reveal } from "../lib/reveal";
  import { STICK_INFO_COMMAND, isUuid, knownStickUuid, stickKeyCheckCommand, stickKeyCopyCommand } from "../lib/derive";
  import { t } from "../lib/locale";
  import type { TargetStatus } from "../lib/types";

  /**
   * The stick's key copy needs its password re-check (or has no confirmed copy yet). Both happen
   * in a terminal as root; the API never takes a password. The UUID of the stick is taken from the
   * newest stick run that names it, otherwise the user enters it (it is not secret).
   */
  let { stick }: { stick: TargetStatus } = $props();

  const known = $derived(knownStickUuid(app.runsOf("stick").value, stick));
  let typed = $state("");
  const uuid = $derived(known ?? typed);
  const bad = $derived(!known && typed.trim() !== "" && !isUuid(typed));
</script>

<section class="pw notice-box" aria-labelledby="h-stickpw">
  <h2 id="h-stickpw"><Dot tone="warn" />{t.stick.passwordTitle}</h2>
  <p>{t.stick.passwordText}</p>
  {#if !known}
    <div class="field">
      <label for="stick-uuid">{t.stick.uuidLabel}</label>
      <input
        id="stick-uuid" class="input" autocomplete="off" spellcheck="false" placeholder="01234567-89ab-cdef-0123-456789abcdef"
        bind:value={typed} aria-invalid={bad ? "true" : undefined}
        aria-describedby={bad ? "stick-uuid-hint stick-uuid-bad" : "stick-uuid-hint"}
      />
      <p class="hint" id="stick-uuid-hint">{t.stick.uuidHint}</p>
      {#if bad}<p class="hint bad" id="stick-uuid-bad" use:reveal>{t.stick.uuidInvalid}</p>{/if}
    </div>
    <Cmd command={STICK_INFO_COMMAND} />
  {/if}
  <p class="lead-in">{t.stick.passwordCmd}</p>
  <Cmd command={stickKeyCheckCommand(uuid)} missing={t.stick.fillUuid} />
  <Cmd command={stickKeyCopyCommand(uuid)} missing={t.stick.fillUuid} />
</section>

<style>
  .pw { margin-top: 22px; max-width: 720px; padding: 14px 16px 16px; }
  h2 { font-size: var(--fs-m); font-weight: 600; display: flex; align-items: center; gap: 10px; }
  p { color: var(--muted); margin-top: 6px; max-width: 66ch; }
  .lead-in { color: var(--text); margin-top: 12px; }
  .field { margin-top: 12px; max-width: 420px; }
  .field label { display: block; font-weight: 500; font-size: 13px; margin-bottom: 4px; }
  .field .hint { margin-top: 4px; font-size: var(--fs-s); }
  .input {
    width: 100%; background: var(--field); border: 1px solid var(--field-border); border-radius: 6px;
    padding: 7px 10px; min-height: 36px; font-family: var(--mono); font-size: 13.5px;
  }
  .input:focus-visible { outline: 2px solid var(--accent-text); outline-offset: 1px; }
  .input[aria-invalid="true"] { border-color: var(--bad); }
</style>
