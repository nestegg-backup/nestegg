// The mock offers every new state and endpoint, shaped like api/openapi.yaml.
import { describe, expect, it } from "vitest";
import { World } from "../mock/world";
import type { PbsCheckResult, PbsInfo, Restore, Run, Snapshot, Status } from "../src/lib/types";

const params = new URLSearchParams();
const status = (w: World, t?: number) => w.handle("GET", "/status", params, null, t).body as Status;
const runs = (w: World, t?: number) => (w.handle("GET", "/runs", params, null, t).body as { runs: Run[] }).runs;

describe("scenario incomplete", () => {
  it("health incomplete; the reasons are also warnings; older backups stay complete", () => {
    const w = new World("incomplete", "full");
    const s = status(w);
    expect(s.health).toBe("incomplete");
    expect(s.last_success?.incomplete?.length).toBeGreaterThan(0);
    for (const reason of s.last_success!.incomplete!) expect(s.last_success!.warnings).toContain(reason);
    const done = runs(w).filter((r) => r.state === "succeeded");
    expect(done.slice(0, 3).every((r) => r.incomplete?.length)).toBe(true);
    expect(done.slice(3).some((r) => r.incomplete?.length)).toBe(false);
  });

  it("failing takes precedence over incomplete", () => {
    const t0 = Date.now();
    const w = new World("incomplete", "full", t0);
    w.handle("POST", "/runs", params, null, t0);
    // A run that ends with a configuration error: the last run failed, the last success is still incomplete.
    w.keyExported = false;
    w.active = null;
    w.runs.unshift({ ...w.runs[0]!, id: "x", state: "failed", snapshot: null, incomplete: [], error: { kind: "config", message: "x" } });
    expect(status(w, t0).health).toBe("failing");
  });
});

describe("verified", () => {
  it("recent runs carry verified=true; older records and the `unverified` fault leave it out", () => {
    const w = new World("ok", "full");
    const list = runs(w).filter((r) => r.state === "succeeded");
    expect(list[0]!.verified).toBe(true);
    expect(list[list.length - 1]!.verified).toBeUndefined();
    w.faults.add("unverified");
    expect(runs(w).every((r) => r.verified === undefined)).toBe(true);
    expect(status(w).last_success?.verified).toBeUndefined();
  });
});

describe("scenario verification_failed", () => {
  it("the last run failed with the new error kind", () => {
    const s = status(new World("verification_failed", "full"));
    expect(s.health).toBe("failing");
    expect(s.last_run?.error?.kind).toBe("verification_failed");
  });
});

describe("scenario damaged", () => {
  it("names the damaged snapshots in /status and marks them in the snapshot list", () => {
    const w = new World("damaged", "full");
    const s = status(w);
    expect(s.health).toBe("ok");
    expect(s.server_verify_failed).toHaveLength(2);
    const snaps = (w.handle("GET", "/snapshots", params, null).body as { snapshots: Snapshot[] }).snapshots;
    expect(snaps.filter((x) => x.verified === "failed").map((x) => x.snapshot).sort()).toEqual([...s.server_verify_failed!].sort());
  });

  it("other scenarios have no such list", () => {
    expect(status(new World("ok", "full")).server_verify_failed).toBeUndefined();
  });
});

describe("scenario no_key", () => {
  it("is set up, but has no key yet", () => {
    const s = status(new World("no_key", "full"));
    expect(s.health).toBe("never");
    expect(s.encryption).toMatchObject({ enabled: true, key_fingerprint: null, key_exported: false });
  });
});

describe("cancel a run", () => {
  it("answers 404 for an unknown run and 409 while the run is still starting", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    expect(w.handle("POST", "/runs/nope/cancel", params, null, t0).status).toBe(404);
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    const early = w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 500);
    expect(early.status).toBe(409);
    expect((early.body as { code: string }).code).toBe("cannot_cancel");
  });

  it("202 while uploading, then the run ends as cancelled and is no longer current", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    const r = w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 10_000);
    expect(r.status).toBe(202);
    expect((r.body as { cancelling: boolean }).cancelling).toBe(true);
    expect(status(w, t0 + 10_500).current_run).not.toBeNull();
    const after = status(w, t0 + 12_000);
    expect(after.current_run).toBeNull();
    expect(after.last_run).toMatchObject({ state: "cancelled", error: { kind: "cancelled" } });
    expect(after.last_success?.id).not.toBe(after.last_run?.id);
    expect(after.health).toBe("ok");
  });

  it("answers 409 when the run is already finishing, and with the cancel_late fault", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    expect(w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 19_000).status).toBe(409);
    const late = new World("ok", "full", t0);
    late.faults.add("cancel_late");
    const r2 = late.handle("POST", "/runs", params, null, t0).body as Run;
    expect(late.handle("POST", `/runs/${r2.id}/cancel`, params, null, t0 + 10_000).status).toBe(409);
  });
});

describe("POST /pbs/check", () => {
  it("returns the checks with a time; waking makes it take a while", () => {
    const w = new World("ok", "full");
    const r = w.handle("POST", "/pbs/check", params, null);
    expect(r.status).toBe(200);
    const body = r.body as PbsCheckResult;
    expect(body.checks.every((c) => typeof c.name === "string" && typeof c.ok === "boolean" && typeof c.detail === "string")).toBe(true);
    expect(body.checks.every((c) => c.ok)).toBe(true);
    expect(Number.isNaN(Date.parse(body.checked_at))).toBe(false);
    expect(r.delayMs).toBeUndefined();
    expect(w.handle("POST", "/pbs/check", new URLSearchParams("wake=true"), null).delayMs).toBeGreaterThan(1000);
  });

  it("finds a problem with the fault, answers 503 when the server is down", () => {
    const bad = new World("ok", "full");
    bad.faults.add("check_bad");
    expect((bad.handle("POST", "/pbs/check", params, null).body as PbsCheckResult).checks.some((c) => !c.ok)).toBe(true);
    const down = new World("ok", "full");
    down.faults.add("check_down");
    const r = down.handle("POST", "/pbs/check", params, null);
    expect(r.status).toBe(503);
    expect((r.body as { code: string }).code).toBe("pbs_unreachable");
    expect(new World("overdue", "full").handle("POST", "/pbs/check", params, null).status).toBe(503);
  });

  it("is not offered in the 'today' flavour", () => {
    expect(new World("ok", "today").handle("POST", "/pbs/check", params, null).status).toBe(404);
  });
});

describe("GET /pbs", () => {
  it("tells the datastore usage unless the fault says the daemon cannot", () => {
    const w = new World("ok", "full");
    const info = w.handle("GET", "/pbs", params, null).body as PbsInfo;
    expect(info.used_bytes).toBeGreaterThan(0);
    expect(info.total_bytes).toBeGreaterThan(info.used_bytes!);
    w.faults.add("no_usage");
    expect((w.handle("GET", "/pbs", params, null).body as PbsInfo).used_bytes).toBeNull();
  });
});

describe("restores with several archives and progress", () => {
  const t0 = Date.now();
  const base = { snapshot_time: "x", target_dir: "/home/alice/new" };

  it("accepts `archives`, rejects a repeated archive", () => {
    const w = new World("ok", "full", t0);
    const ok = w.handle("POST", "/restores", params, { ...base, archives: [{ archive: "home.pxar", paths: ["/alice"] }, { archive: "etc.pxar", paths: [] }] }, t0);
    expect(ok.status).toBe(202);
    const dup = w.handle("POST", "/restores", params, { ...base, archives: [{ archive: "home.pxar", paths: [] }, { archive: "home.pxar", paths: [] }] }, t0);
    expect(dup.status).toBe(400);
    expect(w.handle("POST", "/restores", params, base, t0).status).toBe(400);
  });

  it("reports restored_bytes growing up to total_bytes", () => {
    const w = new World("ok", "full", t0);
    const r = w.handle("POST", "/restores", params, { ...base, archive: "home.pxar", paths: [] }, t0).body as Restore;
    const at = (ms: number) => w.handle("GET", `/restores/${r.id}`, params, null, t0 + ms).body as Restore;
    expect(at(0).restored_bytes).toBe(0);
    const mid = at(2_000);
    expect(mid.state).toBe("running");
    expect(mid.restored_bytes).toBeGreaterThan(0);
    expect(mid.restored_bytes).toBeLessThan(mid.total_bytes!);
    expect(at(5_000)).toMatchObject({ state: "succeeded" });
  });
});

describe("run progress", () => {
  it("offers the estimate (expected_bytes) and the archives finished so far while uploading", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    w.handle("POST", "/runs", params, null, t0);
    const s = status(w, t0 + 14_000).current_run!;
    expect(s.state).toBe("uploading");
    expect(s.expected_bytes).toBeGreaterThan(0);
    expect(s.processed_bytes).toBeLessThanOrEqual(s.expected_bytes!);
  });
});
