<script lang="ts">
  import Dot from "./Dot.svelte";
  import { app } from "../lib/store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";

  // The quiet default line: what the last check of the backup server said.
  const line = $derived.by((): { text: string; tone: "neutral" | "pbs" | "warn" } | null => {
    if (app.fatal) return { text: app.fatal.kind === "unauthorized" ? t.sbar.sessionEnded : t.sbar.offline, tone: "warn" };
    const p = app.status?.pbs;
    if (!p) return null;
    if (p.reachable == null || !p.checked_at) return { text: t.sbar.serverUnknown, tone: "neutral" };
    const m = f.moment(p.checked_at, app.now);
    const day = f.dayMonth(p.checked_at);
    return p.reachable
      ? { text: t.sbar.serverReachable(m, day), tone: "neutral" }
      : { text: t.sbar.serverUnreachable(m, day), tone: "pbs" };
  });
  const msg = $derived(statusBar.message);
</script>

<footer class="sbar">
  <!-- Messages (saved settings) are announced; the default line is not. -->
  <span class="msg" class:ok={msg?.tone === "ok"} class:bad={msg?.tone === "bad"} role="status">{msg?.text ?? ""}</span>
  {#if !msg && line}
    <span class="line">{#if line.tone !== "neutral"}<Dot tone={line.tone} />{/if}{line.text}</span>
  {/if}
  {#if app.healthInfo}<span class="ver">{t.app.version(app.healthInfo.version)}</span>{/if}
</footer>

<style>
  .sbar {
    grid-column: 1 / -1; position: sticky; bottom: 0; z-index: 10; height: var(--sbar-h);
    display: flex; align-items: center; gap: 16px; padding: 0 18px;
    background: var(--side); border-top: 1px solid var(--rule); font-size: var(--fs-s); color: var(--muted);
  }
  /* Stays in the accessibility tree while empty, so the first message is announced. */
  .msg:empty { position: absolute; }
  .msg, .line { display: flex; align-items: center; gap: 8px; min-width: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .msg.ok { color: var(--text); }
  .msg.ok::before {
    content: ""; flex: none; width: 9px; height: 5px; border-left: 2px solid var(--ok); border-bottom: 2px solid var(--ok);
    transform: rotate(-45deg) translateY(-2px);
  }
  .msg.bad { color: var(--bad); }
  .ver { margin-left: auto; flex: none; }
  @media (max-width: 760px) {
    .sbar { position: fixed; left: 0; right: 0; padding: 0 16px; }
    .ver { display: none; }
  }
</style>
