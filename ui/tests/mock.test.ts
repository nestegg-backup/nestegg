// The development mock must stay consistent with the contract the UI relies on.
import { describe, expect, it } from "vitest";
import { SCENARIOS, World } from "../mock/world";
import type { ConfigView, Run, Status } from "../src/lib/types";

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
      incomplete: "incomplete",
      verification_failed: "failing",
      damaged: "ok",
      no_key: "never",
      cancelled: "ok",
    };
    expect(s.health).toBe(expected[sc]);
    expect(s.encryption.key_exported).toBe(sc !== "key_not_exported" && sc !== "unconfigured" && sc !== "no_key");
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

  it("changes settings like the daemon and reflects them in /config and /status", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    const before = w.handle("GET", "/config", params, null, t0).body as ConfigView;
    expect(before.interval_hours).toBe(24);
    expect(before.automatic_backups).toBe(true);
    expect(before.wake_on_lan).toBe(true);
    expect(before.editable).toContain("wake_on_lan");
    const r = w.handle(
      "PATCH",
      "/config",
      params,
      { interval_hours: 12, overdue_after_hours: 20, excludes: ["**/.cache", "*.iso"], wake_on_lan: false, automatic_backups: false },
      t0,
    );
    expect(r.status).toBe(200);
    const after = r.body as ConfigView;
    expect(after).toMatchObject({ interval_hours: 12, schedule: "every 12 h", overdue_after_hours: 20, excludes: ["**/.cache", "*.iso"], wake_on_lan: false, automatic_backups: false });
    expect(w.handle("GET", "/config", params, null, t0).body).toEqual(after);
    const st = w.handle("GET", "/status", params, null, t0).body as Status;
    expect(st.schedule).toMatchObject({ enabled: false, interval_hours: 12, next_run: null });
    expect(st.pbs.wake_on_lan).toBe(false);
    expect(st.overdue_after_hours).toBe(20);
    // Switching the timer back on brings back the next run.
    w.handle("PATCH", "/config", params, { automatic_backups: true }, t0);
    expect((w.handle("GET", "/status", params, null, t0).body as Status).schedule.next_run).toBeTruthy();
  });

  it("rejects settings changes like the daemon and changes nothing", () => {
    const w = new World("ok", "full");
    const before = w.handle("GET", "/config", params, null).body;
    for (const [body, text] of [
      [{ interval_hours: 0 }, "between 1 and 168"],
      [{ interval_hours: 48 }, "greater than interval_hours"],
      [{ excludes: ["**"] }, "exclude everything"],
      [{ excludes: ["a", "a"] }, "listed twice"],
      [{ server: "pbs.example.com" }, "cannot be changed here"],
      [{ bogus: true }, "unknown setting"],
      [{ interval_hours: "12" }, "invalid settings"],
      [null, "JSON object"],
    ] as const) {
      const r = w.handle("PATCH", "/config", params, body);
      expect(r.status, JSON.stringify(body)).toBe(400);
      expect((r.body as { code: string; message: string }).code).toBe("bad_request");
      expect((r.body as { message: string }).message).toContain(text);
    }
    expect(w.handle("GET", "/config", params, null).body).toEqual(before);

    const noWake = new World("ok", "full");
    noWake.faults.add("no_wake");
    expect((noWake.handle("GET", "/config", params, null).body as ConfigView).editable).not.toContain("wake_on_lan");
    expect(noWake.handle("PATCH", "/config", params, { wake_on_lan: true }).status).toBe(400);
    expect(noWake.handle("PATCH", "/config", params, { wake_on_lan: false }).status).toBe(200);

    const failing = new World("ok", "full");
    failing.faults.add("timer_fails");
    const r = failing.handle("PATCH", "/config", params, { interval_hours: 12, automatic_backups: false });
    expect(r.status).toBe(500);
    expect((failing.handle("GET", "/config", params, null).body as ConfigView).interval_hours).toBe(24);
    expect(new World("unconfigured", "full").handle("PATCH", "/config", params, { interval_hours: 12 }).status).toBe(500);
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
