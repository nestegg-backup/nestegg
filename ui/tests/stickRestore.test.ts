// Restoring files from the backup stick: the request carries the source, the stick's own errors
// are told apart from the server's, and nothing claims progress the stick does not report.

import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiFailure, api } from "../src/lib/api";
import { isStickProblem, restore, restoreRequest, stickNotSupported, type TreeNode } from "../src/lib/restore.svelte";
import { stickLine } from "../src/lib/text";
import { createFormatter } from "../src/lib/format";
import de from "../src/i18n/de";
import { World } from "../mock/world";
import type { Restore, Snapshot, Status, TargetStatus } from "../src/lib/types";

const params = new URLSearchParams();
const stickParams = new URLSearchParams("target=stick");
const sel = (archive: string, path: string) => ({ archive, path, size: null });

function serve(w: World) {
  const urls: string[] = [];
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    const u = new URL(url, "http://localhost/");
    const path = u.pathname.replace(/^\/api\/v1/, "");
    urls.push(`${init?.method ?? "GET"} ${path}${u.search}`);
    const r = w.handle(init?.method ?? "GET", path, u.searchParams, init?.body ? JSON.parse(String(init.body)) : null);
    return new Response(JSON.stringify(r.body), { status: r.status });
  });
  return urls;
}

afterEach(() => {
  vi.unstubAllGlobals();
  restore.reset();
  restore.setSource("pbs");
});

describe("the request names the stick, the server's stays as it was", () => {
  it("adds target=stick to the body only for the stick", () => {
    expect(restoreRequest([sel("home.pxar", "/alice/a")], "t", "/x")).not.toHaveProperty("target");
    expect(restoreRequest([sel("home.pxar", "/alice/a")], "t", "/x", "pbs")).not.toHaveProperty("target");
    expect(restoreRequest([sel("home.pxar", "/alice/a")], "t", "/x", "stick")).toEqual({
      target: "stick",
      snapshot_time: "t",
      archive: "home.pxar",
      paths: ["/alice/a"],
      target_dir: "/x",
    });
    const many = restoreRequest([sel("home.pxar", "/"), sel("etc.pxar", "/")], "t", "/x", "stick");
    expect(many.target).toBe("stick");
    expect(many.archives).toHaveLength(2);
  });

  it("asks for snapshots and folders of the stick with target=stick, and of the server without it", async () => {
    const urls = serve(new World("both", "full"));
    await api.snapshots();
    await api.snapshots("stick");
    const snap = (new World("both", "full").stickSnapshots()[0] as Snapshot).time;
    await api.browse(snap, "home.pxar", "/", "stick");
    expect(urls[0]).toBe("GET /snapshots");
    expect(urls[1]).toBe("GET /snapshots?target=stick");
    expect(urls[2]).toMatch(/\/browse\?archive=home\.pxar&path=%2F&target=stick$/);
  });

  it("503 stick_unavailable is the stick's problem, not an unreachable server", () => {
    const f = new ApiFailure("stick_unavailable", 503, "stick_unavailable", "registered stick not plugged in");
    expect(isStickProblem(f)).toBe(true);
    expect(stickNotSupported(f)).toBe(false);
    // A daemon that ignores target=stick answers like for the server: that answer is not about the stick.
    expect(stickNotSupported(new ApiFailure("pbs_unreachable", 503, "pbs_unreachable", "PBS is not reachable"))).toBe(true);
    expect(isStickProblem(new ApiFailure("pbs_unreachable", 503, "pbs_unreachable", "x"))).toBe(false);
  });
});

describe("mock: the stick as the daemon answers", () => {
  it("lists, browses and restores from a plugged-in stick", () => {
    const t0 = Date.now();
    const w = new World("both", "full", t0);
    const snaps = (w.handle("GET", "/snapshots", stickParams, null, t0).body as { snapshots: Snapshot[] }).snapshots;
    expect(snaps.length).toBeGreaterThan(0);
    // The stick's snapshots are its own, not the server's.
    const server = (w.handle("GET", "/snapshots", params, null, t0).body as { snapshots: Snapshot[] }).snapshots;
    expect(snaps.map((s) => s.time)).not.toEqual(server.map((s) => s.time));
    const q = new URLSearchParams({ target: "stick", archive: "home.pxar", path: "/" });
    expect(w.handle("GET", `/snapshots/${snaps[0]!.time}/browse`, q, null, t0).status).toBe(200);
    const started = w.handle("POST", "/restores", params, { target: "stick", snapshot_time: snaps[0]!.time, archive: "home.pxar", paths: ["/alice"], target_dir: "/home/alice/neu" }, t0);
    expect(started.status).toBe(202);
    const id = (started.body as Restore).id;
    const mid = w.handle("GET", `/restores/${id}`, params, null, t0 + 3_000).body as Restore;
    // No progress while it runs, and never a total.
    expect(mid.state).toBe("running");
    expect(mid.restored_bytes).toBe(0);
    expect(mid.total_bytes).toBeNull();
    const end = w.handle("GET", `/restores/${id}`, params, null, t0 + 9_000).body as Restore;
    expect(end.state).toBe("succeeded");
    expect(end.restored_bytes).toBeGreaterThan(0);
    expect(end.total_bytes).toBeNull();
  });

  it("an absent or pulled stick answers 503 stick_unavailable, and a running restore fails", () => {
    const t0 = Date.now();
    expect(new World("both_absent", "full", t0).handle("GET", "/snapshots", stickParams, null, t0).body).toMatchObject({ code: "stick_unavailable" });
    const w = new World("stick_only", "full", t0);
    const time = w.stickSnapshots()[0]!.time;
    const id = (w.handle("POST", "/restores", params, { target: "stick", snapshot_time: time, archive: "home.pxar", paths: [], target_dir: "/home/alice/neu" }, t0).body as Restore).id;
    w.faults.add("stick_pulled");
    expect((w.handle("GET", "/status", params, null, t0 + 1_000).body as Status).targets!.stick!.plugged_in).toBe(false);
    const job = w.handle("GET", `/restores/${id}`, params, null, t0 + 1_000).body as Restore;
    expect(job.state).toBe("failed");
    expect(job.error).toMatch(/removed/);
    const q = new URLSearchParams({ target: "stick", archive: "home.pxar", path: "/" });
    const r = w.handle("GET", `/snapshots/${time}/browse`, q, null, t0 + 1_000);
    expect(r.status).toBe(503);
    expect(r.body).toMatchObject({ code: "stick_unavailable" });
  });

  it("a daemon from before stick restores ignores target=stick", () => {
    const w = new World("stick_only", "today");
    expect(w.handle("GET", "/snapshots", stickParams, null).body).toMatchObject({ code: "pbs_unreachable" });
  });
});

describe("restore state with the stick as source", () => {
  it("switching the source starts over", () => {
    restore.snapshotTime = "2026-11-10T18:10:00Z";
    restore.selection = [sel("home.pxar", "/alice")];
    restore.setSource("stick");
    expect(restore.source).toBe("stick");
    expect(restore.snapshotTime).toBeNull();
    expect(restore.selection).toEqual([]);
  });

  it("reads the stick's snapshots, and keeps a refusal as the stick's error", async () => {
    const w = new World("both", "full");
    serve(w);
    restore.setSource("stick");
    await restore.loadStickSnapshots();
    expect(restore.stickSnaps.state).toBe("ok");
    expect(restore.snaps({ state: "idle", list: [], error: null }).list).toEqual(restore.stickSnaps.list);
    w.faults.add("stick_pulled");
    await restore.loadStickSnapshots(true);
    expect(restore.stickSnaps.state).toBe("error");
    expect(restore.stickSnaps.error?.kind).toBe("stick_unavailable");
  });

  it("a stick pulled out mid-browse marks the folder and says it is the stick", async () => {
    const w = new World("stick_only", "full");
    serve(w);
    restore.setSource("stick");
    await restore.loadStickSnapshots();
    const snap = restore.stickSnaps.list[0]!;
    restore.pick(snap.time);
    await restore.prepare(snap, null);
    expect(restore.stickProblem).toBeNull();
    const home = restore.roots.find((r) => r.archive === "home.pxar")!;
    // Without a configuration the archives are sorted by name: open the home archive here.
    if (!home.children) await restore.toggle(home);
    const alice = home.children!.find((c) => c.name === "alice") as TreeNode;
    w.faults.add("stick_pulled");
    await restore.toggle(alice);
    expect(alice.failed).toBe(true);
    expect(restore.stickProblem?.code).toBe("stick_unavailable");
    // Plugged in again: opening the folder again works and the note goes away.
    w.faults.delete("stick_pulled");
    await restore.toggle(alice);
    expect(alice.children?.length).toBeGreaterThan(0);
    expect(restore.stickProblem).toBeNull();
  });

  it("starting with the stick gone is reported as the stick's problem; a started job remembers its source", async () => {
    const w = new World("stick_only", "full");
    const urls = serve(w);
    restore.setSource("stick");
    await restore.loadStickSnapshots();
    restore.pick(restore.stickSnaps.list[0]!.time);
    restore.selection = [sel("home.pxar", "/alice/Dokumente")];
    restore.fullPath = "/home/alice/neu";
    w.faults.add("stick_pulled");
    expect(await restore.start(null)).toBe(false);
    expect(restore.startError?.stick).toBe(true);
    w.faults.delete("stick_pulled");
    expect(await restore.start(null)).toBe(true);
    expect(restore.jobSource).toBe("stick");
    expect(restore.usingStick).toBe(true);
    expect(urls.filter((u) => u.startsWith("POST /restores"))).toHaveLength(2);
  });
});

describe("the stick row while a restore reads from it", () => {
  const f = createFormatter("de", "de-DE", "Europe/Berlin");
  const now = new Date("2026-11-11T18:00:00Z");
  const ts: TargetStatus = {
    health: "ok",
    state: "ok",
    last_run: { id: "r", target: "stick", trigger: "plug", state: "succeeded", started_at: "2026-11-10T18:10:00Z" },
    last_success: { id: "r", target: "stick", trigger: "plug", state: "succeeded", started_at: "2026-11-10T18:10:00Z" },
    current_run: null,
    consecutive_failures: 0,
    overdue_after_hours: 168,
    password_reminder: false,
    plugged_in: true,
  };

  it("never says it can be unplugged while this page restores from it", () => {
    expect(stickLine(ts, de, f, now).plug).toBe(de.stick.safe);
    const busy = stickLine(ts, de, f, now, true);
    expect(busy.view.safeToUnplug).toBe(false);
    expect(busy.plug).toBe(de.stick.keepIn);
  });
});
