// The development mock must stay consistent with the contract the UI relies on.
import { describe, expect, it } from "vitest";
import { SCENARIOS, World } from "../mock/world";
import type { Run, Status } from "../src/lib/types";

const params = new URLSearchParams();

describe("mock API", () => {
  it.each(SCENARIOS.filter((s) => s !== "daemon_down" && s !== "unauthorized"))("scenario %s answers /status", (sc) => {
    const w = new World(sc, "full");
    const r = w.handle("GET", "/status", params, null);
    expect(r.status).toBe(200);
    const s = r.body as Status;
    const expected: Record<string, Status["health"]> = {
      ok: "ok",
      running: "running",
      overdue: "overdue",
      failing: "failing",
      never: "never",
      reinstalled: "never",
      unconfigured: "unconfigured",
      pbs_unreachable: "ok",
      key_not_exported: "never",
      credentials_unreadable: "failing",
    };
    expect(s.health).toBe(expected[sc]);
    expect(s.encryption.key_exported).toBe(sc !== "key_not_exported" && sc !== "unconfigured");
  });

  it("a started run goes through all phases and succeeds", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    expect(w.handle("POST", "/runs", params, null, t0).status).toBe(202);
    expect(w.handle("POST", "/runs", params, null, t0 + 100).status).toBe(409);
    const seen = new Set<string>();
    for (let t = t0; t < t0 + 25_000; t += 500) {
      const s = w.handle("GET", "/status", params, null, t).body as Status;
      seen.add(s.current_run?.state ?? "none");
    }
    expect([...seen]).toEqual(["starting", "waking_pbs", "inventory", "uploading", "finishing", "none"]);
    const runs = (w.handle("GET", "/runs", params, null, t0 + 25_000).body as { runs: Run[] }).runs;
    expect(runs[0]!.state).toBe("succeeded");
    expect(runs[0]!.trigger).toBe("manual");
  });

  it("offers only today's endpoints in the 'today' flavour", () => {
    const w = new World("ok", "today");
    expect(w.handle("GET", "/machines", params, null).status).toBe(404);
    expect(w.handle("GET", "/session", params, null).status).toBe(404);
    const snap = (w.handle("GET", "/snapshots", params, null).body as { snapshots: { time: string }[] }).snapshots[0]!;
    expect(w.handle("GET", `/snapshots/${snap.time}/browse`, new URLSearchParams("archive=home.pxar"), null).status).toBe(501);
  });

  it("reinstalled: no local history, but the snapshots stay on the server", () => {
    const t0 = Date.now();
    const w = new World("reinstalled", "full", t0);
    const st = w.handle("GET", "/status", params, null, t0).body as Status;
    expect(st.health).toBe("never");
    expect(st.last_success).toBeNull();
    expect(st.pbs.checked_at).toBeNull();
    expect((w.handle("GET", "/runs", params, null, t0).body as { runs: Run[] }).runs).toEqual([]);
    const before = (w.handle("GET", "/snapshots", params, null, t0).body as { snapshots: { time: string }[] }).snapshots;
    expect(before.length).toBeGreaterThan(5);
    // The first backup of the new installation adds one snapshot and keeps the old ones.
    w.handle("POST", "/runs", params, null, t0);
    const after = (w.handle("GET", "/snapshots", params, null, t0 + 25_000).body as { snapshots: { time: string }[] }).snapshots;
    expect(after.length).toBe(before.length + 1);
    expect(after.slice(1)).toEqual(before);
    expect((w.handle("GET", "/status", params, null, t0 + 25_000).body as Status).health).toBe("ok");
  });

  it("refuses to restore into an existing folder", () => {
    const w = new World("ok", "full");
    const body = { snapshot_time: "x", archive: "home.pxar", paths: [], target_dir: "/home/alice/Dokumente" };
    expect(w.handle("POST", "/restores", params, body).status).toBe(409);
    expect(w.handle("POST", "/restores", params, { ...body, target_dir: "/home/alice/new" }).status).toBe(202);
  });

  it("uses placeholder data only", () => {
    const w = new World("failing", "full");
    const all = JSON.stringify([
      w.handle("GET", "/status", params, null).body,
      w.handle("GET", "/config", params, null).body,
      w.handle("GET", "/machines", params, null).body,
    ]);
    const names = (all.match(/[a-z0-9-]+(?:\.[a-z0-9-]+)+/gi) ?? []).filter((n) => /\.[a-z]{2,}$/i.test(n));
    expect(names.length).toBeGreaterThan(0);
    for (const name of names) {
      if (/\.(pxar|didx|blob|pdf|odt|jpg|toml|json|txt|sh|m3u|deb|dirs)$/i.test(name)) continue;
      expect(name).toMatch(/(^|\.)example\.com$/);
    }
  });
});
