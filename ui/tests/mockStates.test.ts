// The mock offers every new state and endpoint, shaped like api/openapi.yaml.
import { describe, expect, it } from "vitest";
import { World } from "../mock/world";
import { setupGuide } from "../src/lib/derive";
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

describe("scenarios for every step of the guide", () => {
  const stepOf = (name: World["scenario"], faults: string[] = []) => {
    const w = new World(name, "full");
    for (const f of faults) w.faults.add(f as never);
    return setupGuide(status(w))?.current ?? null;
  };

  it("lead to the step they are named after", () => {
    expect(stepOf("unconfigured")).toBe("connect");
    expect(stepOf("no_credentials")).toBe("credentials");
    expect(stepOf("no_key")).toBe("key_create");
    expect(stepOf("key_not_exported")).toBe("key_store");
    expect(stepOf("no_timer")).toBe("automatic");
    expect(stepOf("never")).toBe("first_backup");
    expect(stepOf("ok")).toBeNull();
  });

  it("send the new fields: credentials_stored and key_present", () => {
    expect(status(new World("no_credentials", "full")).pbs.credentials_stored).toBe(false);
    expect(status(new World("no_credentials", "full")).encryption.key_present).toBe(false);
    expect(status(new World("no_key", "full")).pbs.credentials_stored).toBe(true);
    expect(status(new World("no_key", "full")).encryption.key_present).toBe(false);
    expect(status(new World("key_not_exported", "full")).encryption).toMatchObject({ key_present: true, key_exported: false });
    expect(status(new World("ok", "full")).encryption).toMatchObject({ key_present: true, key_exported: true });
  });

  it("the faults make the values unknown or switch encryption off", () => {
    const w = new World("never", "full");
    w.faults.add("unknown_stored");
    expect(status(w).pbs.credentials_stored).toBeNull();
    expect(status(w).encryption.key_present).toBeNull();
    const plain = new World("never", "full");
    plain.faults.add("no_encryption");
    expect(status(plain).encryption).toMatchObject({ enabled: false, key_exported: false, key_fingerprint: null });
    expect(plain.handle("GET", "/config", params, null).body).toMatchObject({ encryption: false });
    expect(setupGuide(status(plain))?.steps).not.toContain("key_create");
  });

  it("switching automatic backups on moves the guide on", () => {
    const w = new World("no_timer", "full");
    expect(w.handle("PATCH", "/config", params, { automatic_backups: true }).status).toBe(200);
    expect(status(w).schedule.enabled).toBe(true);
    expect(setupGuide(status(w))?.current).toBe("first_backup");
  });

  it("a backup without the token secret fails like the daemon, with a message about the secret", () => {
    const t0 = Date.now();
    const w = new World("no_credentials", "full", t0);
    w.handle("POST", "/runs", params, null, t0);
    const after = status(w, t0 + 3_000);
    expect(after.last_run).toMatchObject({ state: "failed", error: { kind: "config" } });
    expect(after.last_run?.error?.message).toMatch(/token secret/);
  });
});

describe("cancel a run", () => {
  // crates/nestegg/src/daemon.rs, cancel_run: 404 for an id that does not exist, 409 `cannot_cancel`
  // for a run that exists but is still starting or already over (the contract in api/openapi.yaml).
  it("answers 404 for an unknown run and 409 while the run is still starting", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    const unknown = w.handle("POST", "/runs/nope/cancel", params, null, t0);
    expect(unknown.status).toBe(404);
    expect((unknown.body as { code: string }).code).toBe("not_found");
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    const early = w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 500);
    expect(early.status).toBe(409);
    expect((early.body as { code: string }).code).toBe("cannot_cancel");
    expect((early.body as { message: string }).message).toMatch(/still starting/);
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

  it("answers 409 for a run that is over, and with the cancel_late fault", () => {
    const t0 = Date.now();
    const w = new World("ok", "full", t0);
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    const late = new World("ok", "full", t0);
    late.faults.add("cancel_late");
    const r2 = late.handle("POST", "/runs", params, null, t0).body as Run;
    const r = late.handle("POST", `/runs/${r2.id}/cancel`, params, null, t0 + 10_000);
    expect(r.status).toBe(409);
    expect((r.body as { message: string }).message).toMatch(/already finished/);
    // A run that has finished: also 409, not 404, because the run exists.
    const done = w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 30_000);
    expect(done.status).toBe(409);
    expect((done.body as { code: string }).code).toBe("cannot_cancel");
  });

  it("the 'today' flavour answers 404 in every case, like a daemon from before", () => {
    const t0 = Date.now();
    const w = new World("ok", "today", t0);
    const run = w.handle("POST", "/runs", params, null, t0).body as Run;
    expect(w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 500).status).toBe(404);
    expect(w.handle("POST", "/runs/nope/cancel", params, null, t0).status).toBe(404);
    expect(w.handle("POST", `/runs/${run.id}/cancel`, params, null, t0 + 30_000).status).toBe(404);
  });
});

describe("POST /pbs/check", () => {
  const check = (w: World, query = "") => w.handle("POST", "/pbs/check", new URLSearchParams(query), null);
  const item = (r: ReturnType<typeof check>, name: string) => (r.body as PbsCheckResult).checks.find((c) => c.name === name);

  it("returns the checks of the daemon with a time; waking makes it take a while", () => {
    const w = new World("ok", "full");
    const r = check(w);
    expect(r.status).toBe(200);
    const body = r.body as PbsCheckResult;
    expect(body.checks.every((c) => typeof c.name === "string" && typeof c.ok === "boolean" && typeof c.detail === "string")).toBe(true);
    expect(body.checks.every((c) => c.ok)).toBe(true);
    expect(body.checks.map((c) => c.name)).toEqual(
      expect.arrayContaining(["configuration", "proxmox-backup-client", "password / token secret", "encryption key", "PBS reachable", "PBS login and datastore access", "backup timer enabled"]),
    );
    expect(Number.isNaN(Date.parse(body.checked_at))).toBe(false);
    expect(r.delayMs).toBeUndefined();
    expect(check(w, "wake=true").delayMs).toBeGreaterThan(1000);
  });

  it("a server that is off is an answer, not an error: 200, never 503", () => {
    // No Wake-on-LAN: "no answer" is a failed check, and the login was not checked.
    const off = new World("overdue", "full");
    off.faults.add("no_wake");
    const r = check(off);
    expect(r.status).toBe(200);
    expect(item(r, "PBS reachable")).toMatchObject({ ok: false, status: "failed", detail: "no answer" });
    expect(item(r, "PBS login and datastore access")).toMatchObject({ ok: false, status: "not_checked" });
    expect(item(r, "overview access")).toMatchObject({ status: "not_checked" });
    // With Wake-on-LAN and no wake request nothing was checked: not_checked, never "fine".
    const wol = check(new World("overdue", "full"));
    expect(wol.status).toBe(200);
    expect(item(wol, "PBS reachable")).toMatchObject({ ok: false, status: "not_checked" });
    expect(item(wol, "PBS reachable")!.detail).toMatch(/asleep/);
    expect(item(wol, "PBS login and datastore access")).toMatchObject({ status: "not_checked" });
    // Asked to wake it, and nobody answers: that is a failed check.
    const woken = check(new World("overdue", "full"), "wake=true");
    expect(item(woken, "PBS reachable")).toMatchObject({ ok: false, status: "failed" });
    expect(item(woken, "PBS reachable")!.detail).toMatch(/Wake-on-LAN attempt/);
    expect(item(woken, "PBS login and datastore access")).toMatchObject({ status: "not_checked" });
    // The fault switches a healthy scenario's server off.
    const down = new World("ok", "full");
    down.faults.add("check_down");
    down.faults.add("no_wake");
    for (const q of ["", "wake=true"]) {
      const d = check(down, q);
      expect(d.status).toBe(200);
      expect(item(d, "PBS reachable")?.ok).toBe(false);
    }
  });

  it("every item carries a status that matches ok", () => {
    for (const sc of ["ok", "overdue", "no_credentials"] as const) {
      for (const c of (check(new World(sc, "full")).body as PbsCheckResult).checks) {
        expect(c.status).toBeDefined();
        expect(c.ok).toBe(c.status === "passed");
      }
    }
  });

  it("finds a login problem with the fault", () => {
    const bad = new World("ok", "full");
    bad.faults.add("check_bad");
    expect(item(check(bad), "PBS login and datastore access")?.ok).toBe(false);
  });

  it("reports missing pieces as failed items: token secret, key, key copy, timer", () => {
    const r = check(new World("no_credentials", "full"));
    expect(r.status).toBe(200);
    expect(item(r, "password / token secret")?.ok).toBe(false);
    expect(item(r, "encryption key")?.ok).toBe(false);
    expect(item(r, "encryption key exported")?.ok).toBe(false);
    expect(item(check(new World("no_timer", "full")), "backup timer enabled")?.ok).toBe(false);
    const plain = new World("ok", "full");
    plain.faults.add("no_encryption");
    expect(item(check(plain), "encryption key")).toBeUndefined();
  });

  it("an unreadable configuration is one failed item, also 200", () => {
    const r = check(new World("unconfigured", "full"));
    expect(r.status).toBe(200);
    expect((r.body as PbsCheckResult).checks).toEqual([expect.objectContaining({ name: "configuration", ok: false })]);
  });

  it("does not change what /status says about the server", () => {
    const w = new World("overdue", "full");
    const before = status(w).pbs;
    check(w);
    expect(status(w).pbs).toEqual(before);
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

describe("GET /machines without a configuration", () => {
  it("answers 500 like the daemon (it cannot read the server), with data once configured", () => {
    const none = new World("unconfigured", "full").handle("GET", "/machines", params, null);
    expect(none.status).toBe(500);
    expect((none.body as { code: string }).code).toBe("internal");
    const ok = new World("ok", "full").handle("GET", "/machines", params, null);
    expect(ok.status).toBe(200);
  });
});

describe("GET /machines while the backup server does not answer", () => {
  const machines = (w: World) => w.handle("GET", "/machines", params, null);

  it("answers 503 pbs_unreachable like the daemon where the server is off", () => {
    for (const sc of ["pbs_unreachable", "overdue"] as const) {
      const r = machines(new World(sc, "full"));
      expect(r.status, sc).toBe(503);
      expect(r.body).toMatchObject({ code: "pbs_unreachable" });
    }
    const down = new World("ok", "full");
    down.faults.add("check_down");
    expect(machines(down).status).toBe(503);
  });

  it("answers 200 wherever the server answers or is not known to be off", () => {
    for (const sc of ["ok", "failing", "never", "key_not_exported", "damaged", "credentials_unreadable"] as const)
      expect(machines(new World(sc, "full")).status, sc).toBe(200);
    // Older daemons do not have the endpoint at all.
    expect(machines(new World("pbs_unreachable", "today")).status).toBe(404);
  });
});

describe("PATCH /config limits (decision 0022)", () => {
  const patch = (w: World, body: unknown) => w.handle("PATCH", "/config", params, body);
  type Limits = { overdue_after_hours: { max: number; base: number; per_interval_extra: number; file_minimum: number } };
  const limits = (w: World) => (w.handle("GET", "/config", params, null).body as { limits: Limits }).limits;
  const message = (r: { body: unknown }) => (r.body as { message: string }).message;

  it("reports the numbers the page needs to compute the cap", () => {
    const w = new World("ok", "full");
    expect(limits(w).overdue_after_hours).toMatchObject({ min: 2, max: 168, base: 168, per_interval_extra: 72, file_minimum: 36 });
    expect(patch(w, { interval_hours: 168, overdue_after_hours: 240 }).status).toBe(200);
    expect(limits(w).overdue_after_hours).toMatchObject({ max: 240, base: 168, per_interval_extra: 72, file_minimum: 240 });
  });

  it("refuses 'overdue after' above the cap of the rhythm in the same request", () => {
    const w = new World("ok", "full");
    expect(patch(w, { overdue_after_hours: 168 }).status).toBe(200);
    const r = patch(w, { overdue_after_hours: 169 });
    expect(r.status).toBe(400);
    expect(message(r)).toBe(
      "overdue_after_hours must be between 2 and 168 hours through the web interface. A longer time can only be set as root in /etc/nestegg/config.toml.",
    );
    // The weekly rhythm works: 168 hours is the rhythm, 240 the cap.
    expect(patch(w, { interval_hours: 168, overdue_after_hours: 241 }).status).toBe(400);
    // A weekly rhythm alone does not fit the current 168 hours (it must stay longer than the rhythm).
    expect(message(patch(w, { interval_hours: 168 }))).toContain("must be greater than interval_hours (168)");
    expect(patch(w, { interval_hours: 168, overdue_after_hours: 240 }).status).toBe(200);
    // The 240 in the file stays the limit (it can only be lowered), also for a daily rhythm.
    expect(patch(w, { interval_hours: 24, overdue_after_hours: 241 }).status).toBe(400);
    // Lowered, the cap follows the rhythm of the same request again.
    expect(patch(w, { interval_hours: 24, overdue_after_hours: 100 }).status).toBe(200);
    expect(patch(w, { interval_hours: 24, overdue_after_hours: 200 }).status).toBe(400);
    expect(patch(w, { interval_hours: 96, overdue_after_hours: 168 }).status).toBe(200);
    expect(patch(w, { interval_hours: 120, overdue_after_hours: 192 }).status).toBe(200);
    expect(patch(w, { interval_hours: 120, overdue_after_hours: 193 }).status).toBe(400);
  });

  it("a value from the file above the cap is reported as the maximum and can only be lowered", () => {
    const w = new World("ok", "full");
    w.faults.add("long_overdue");
    const cfg = w.handle("GET", "/config", params, null).body as { overdue_after_hours: number };
    expect(cfg.overdue_after_hours).toBe(744);
    expect(limits(w).overdue_after_hours).toMatchObject({ max: 744, file_minimum: 744 });
    const r = patch(w, { overdue_after_hours: 745 });
    expect(r.status).toBe(400);
    expect(message(r)).toContain("between 2 and 744 hours through the web interface; the current value of 744 can only be lowered here.");
    expect(patch(w, { interval_hours: 12 }).status).toBe(200);
    expect(patch(w, { overdue_after_hours: 100 }).status).toBe(200);
    // Lowered once, the cap is the API's again.
    expect(limits(w).overdue_after_hours.max).toBe(168);
    expect(patch(w, { overdue_after_hours: 200 }).status).toBe(400);
  });

  it("refuses the fixed set of patterns the daemon refuses on a default installation, with its message", () => {
    const w = new World("ok", "full");
    for (const [bad, folder] of [
      ["/alice", "/home/alice"],
      ["alice", "/home/alice"],
      ["**/alice", "/home/alice"],
      ["/[a-z]*", "/home/alice"],
    ] as const) {
      const r = patch(w, { excludes: [...(w.config()!.excludes ?? []), bad] });
      expect(r.status, bad).toBe(400);
      expect(message(r), bad).toBe(
        `this exclude list would leave out the whole folder ${folder} from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.`,
      );
    }
    // Patterns are relative to each archive: these match nothing there.
    for (const fine of ["/home", "/etc/**", "/usr/local"]) {
      expect(patch(new World("ok", "full"), { excludes: [fine] }).status, fine).toBe(200);
    }
    // What the backup client cannot parse is refused with the daemon's words.
    for (const bad of ["[abc", "x[a-]", "foo\\"]) {
      const r = patch(new World("ok", "full"), { excludes: [bad] });
      expect(r.status, bad).toBe(400);
      expect(message(r), bad).toMatch(/is malformed|is not a valid pattern/);
    }
  });

  it("accepts the rest, also what the old rule on spelling refused", () => {
    const w = new World("ok", "full");
    const ok = ["**/.cache", "/home/*/Videos", "**/node_modules", "/Downloads", "/var", "/alice/Downloads", "*.iso"];
    expect(patch(w, { excludes: ok }).status).toBe(200);
    expect(w.config()!.excludes).toEqual(ok);
  });

  it("does not judge a pattern again that is already in the list", () => {
    const w = new World("ok", "full");
    w.excludes = ["/alice", "**/.cache"];
    expect(patch(w, { excludes: ["/alice", "**/.cache", "*.iso"] }).status).toBe(200);
    expect(patch(w, { excludes: ["/alice", "**/.cache", "*.iso", "alice"] }).status).toBe(400);
  });
});

describe("the sleeping server's detail text", () => {
  it("is the daemon's: it names no command (the page offers the wake button)", () => {
    const r = new World("overdue", "full").handle("POST", "/pbs/check", params, null).body as PbsCheckResult;
    const reachable = r.checks.find((c) => c.name === "PBS reachable")!;
    expect(reachable.status).toBe("not_checked");
    expect(reachable.detail).toBe("no answer; the server may be asleep (Wake-on-LAN is configured, a backup run wakes it by itself)");
    expect(reachable.detail).not.toContain("nestegg check");
  });
});
