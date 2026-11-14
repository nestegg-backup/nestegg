<script lang="ts">
  import Dot from "../components/Dot.svelte";
  import { hub } from "./store.svelte";
  import { statusBar } from "../lib/statusbar.svelte";
  import { f, t } from "../lib/locale";

  // The quiet default line: when the hub last read the backup server.
  const line = $derived.by((): { text: string; tone: "neutral" | "pbs" | "warn" } | null => {
    if (hub.phase !== "in") return null;
    if (hub.offline) return { text: t.hub.sbarOffline, tone: "warn" };
    const m = hub.machines;
    if (!m) return null;
    if (!m.pbs_read_at) return { text: t.hub.pbs.never, tone: "pbs" };
    return { text: t.hub.pbs.readAt(f.moment(m.pbs_read_at, hub.now)), tone: m.pbs_reachable ? "neutral" : "pbs" };
  });
  const msg = $derived(statusBar.message);
</script>

<footer class="sbar">
  <span class="msg" class:ok={msg?.tone === "ok"} class:bad={msg?.tone === "bad"} role="status">{msg?.text ?? ""}</span>
  {#if !msg && line}
    <span class="line">{#if line.tone !== "neutral"}<Dot tone={line.tone} />{/if}{line.text}</span>
  {/if}
  {#if hub.phase === "in" && hub.hub}<span class="ver">{t.app.version(hub.hub.version)}</span>{/if}
</footer>

<style>
  .sbar {
    grid-column: 1 / -1; position: sticky; bottom: 0; z-index: 10; height: var(--sbar-h);
    display: flex; align-items: center; gap: 16px; padding: 0 18px;
    background: var(--side); border-top: 1px solid var(--rule); font-size: var(--fs-s); color: var(--muted);
  }
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
