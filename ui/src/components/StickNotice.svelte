<script lang="ts">
  import Cmd from "./Cmd.svelte";
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { reveal } from "../lib/reveal";
  import { STICK_INFO_COMMAND, isUuid, knownStickUuid, registeredSticks, stickKeyCheckCommand, stickKeyCopyCommand, stickName } from "../lib/derive";
  import { stickLabel } from "../lib/text";
  import { t } from "../lib/locale";
  import type { TargetStatus } from "../lib/types";

  /**
   * The stick's key copy needs its password re-check (or has no confirmed copy yet). Both happen
   * in a terminal as root; the API never takes a password. Registered UUIDs and
   * reminder reasons select the needed command; older daemons keep the manual fallback.
   */
  let { stick }: { stick: TargetStatus } = $props();

  const known = $derived(knownStickUuid(app.runsOf("stick").value, stick, app.config.value?.stick_data_uuids));
  let typed = $state("");
  const uuid = $derived(known ?? typed);
  const reason = $derived(stick.password_reminders?.[uuid.toLowerCase()] ?? null);
  const bad = $derived(!known && typed.trim() !== "" && !isUuid(typed));
  // With several sticks, or a named one, say which stick the commands are for.
  const concerns = $derived(
    known && (registeredSticks(stick, app.config.value).length > 1 || stickName(known, stick, app.config.value))
      ? stickLabel(known, stick, app.config.value, t)
      : null,
  );
</script>

<section class="pw notice-box" aria-labelledby="h-stickpw">
  <h2 id="h-stickpw"><Dot tone="warn" />{reason === "key_copy_missing" ? t.stick.keyCopyTitle : t.stick.passwordTitle}</h2>
  <p>{reason === "key_copy_missing" ? t.stick.keyCopyText : reason === "password_check_due" ? t.stick.passwordDueText : t.stick.passwordText}</p>
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
  {#if concerns}<p class="which">{t.stick.concerns(concerns)}</p>{/if}
  <p class="lead-in">{t.stick.passwordCmd}</p>
  {#if reason !== "key_copy_missing"}<Cmd command={stickKeyCheckCommand(uuid)} missing={t.stick.fillUuid} />{/if}
  {#if reason !== "password_check_due"}<Cmd command={stickKeyCopyCommand(uuid)} missing={t.stick.fillUuid} />{/if}
</section>

<style>
  .pw { margin-top: 22px; max-width: 720px; padding: 14px 16px 16px; }
  h2 { font-size: var(--fs-m); font-weight: 600; display: flex; align-items: center; gap: 10px; }
  p { color: var(--muted); margin-top: 6px; max-width: 66ch; }
  .lead-in { color: var(--text); margin-top: 12px; }
  .which { color: var(--text); }
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
