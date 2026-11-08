import { describe, expect, it } from "vitest";
import {
  dayStrip,
  historyRows,
  intervalHours,
  isActive,
  machineKind,
  restorableArchives,
  runEvents,
  runStep,
  selfKind,
  serverContact,
  sortMachines,
} from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import type { Machine, Run, Status } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true },
    encryption: { enabled: true, key_exported: true },
    health: "ok",
    ...over,
  };
}

function run(started_at: string, state: Run["state"], over: Partial<Run> = {}): Run {
  return { id: `r-${started_at}-${state}`, trigger: "timer", state, started_at, ...over };
}

describe("selfKind", () => {
  it("maps every health value", () => {
    expect(selfKind(status({ health: "ok" }))).toBe("ok");
    expect(selfKind(status({ health: "running" }))).toBe("running");
    expect(selfKind(status({ health: "overdue" }))).toBe("overdue");
    expect(selfKind(status({ health: "failing" }))).toBe("failing");
    expect(selfKind(status({ health: "never" }))).toBe("never");
    expect(selfKind(status({ health: "unconfigured" }))).toBe("unconfigured");
  });

  it("puts a missing key copy above failures, but not above a running backup or missing setup", () => {
    const noKey = { enabled: true, key_exported: false };
    expect(selfKind(status({ health: "failing", encryption: noKey }))).toBe("key_missing");
    expect(selfKind(status({ health: "never", encryption: noKey }))).toBe("key_missing");
    expect(selfKind(status({ health: "running", encryption: noKey }))).toBe("running");
    expect(selfKind(status({ health: "unconfigured", encryption: noKey }))).toBe("unconfigured");
    expect(selfKind(status({ health: "ok", encryption: { enabled: false, key_exported: false } }))).toBe("ok");
  });

  it("treats an active current run as running even if health lags behind", () => {
    expect(selfKind(status({ health: "ok", current_run: run("2026-11-08T17:59:00Z", "uploading") }))).toBe("running");
    expect(isActive(run("x", "succeeded"))).toBe(false);
  });
});

describe("dayStrip", () => {
  it("has 14 local days, oldest first, today last", () => {
    const cells = dayStrip([], now, f);
    expect(cells).toHaveLength(14);
    expect(cells[0]!.key).toBe("2026-10-26");
    expect(cells[13]!.key).toBe("2026-11-08");
    expect(cells[13]!.today).toBe(true);
    expect(cells.every((c) => c.kind === "none")).toBe(true);
  });

  it("success wins over failures of the same day; failures over unreachable server", () => {
    const runs = [
      run("2026-11-08T00:31:00Z", "failed"),
      run("2026-11-08T09:00:00Z", "succeeded"),
      run("2026-11-07T00:30:00Z", "pbs_unreachable"),
      run("2026-11-07T01:30:00Z", "failed"),
      run("2026-11-06T00:30:00Z", "pbs_unreachable"),
      run("2026-11-05T00:30:00Z", "cancelled"),
    ];
    const cells = dayStrip(runEvents(runs), now, f);
    const by = Object.fromEntries(cells.map((c) => [c.key, c.kind]));
    expect(by["2026-11-08"]).toBe("ok");
    expect(by["2026-11-07"]).toBe("fail");
    expect(by["2026-11-06"]).toBe("pbs");
    expect(by["2026-11-05"]).toBe("none");
  });

  it("assigns runs to the local day", () => {
    // 23:30 UTC on the 7th is 00:30 on the 8th in Berlin.
    const cells = dayStrip(runEvents([run("2026-11-07T23:30:00Z", "succeeded")]), now, f);
    expect(cells[13]!.kind).toBe("ok");
    expect(cells[12]!.kind).toBe("none");
  });

  it("shows a running backup", () => {
    const cells = dayStrip(runEvents([], run("2026-11-08T17:59:00Z", "uploading")), now, f);
    expect(cells[13]!.kind).toBe("run");
  });
});

describe("historyRows", () => {
  it("folds hourly retries of an unreachable server on the same day", () => {
    const rows = historyRows(
      [
        run("2026-11-08T03:30:00Z", "pbs_unreachable"),
        run("2026-11-08T02:30:00Z", "pbs_unreachable"),
        run("2026-11-08T01:30:00Z", "pbs_unreachable"),
        run("2026-11-07T22:30:00Z", "pbs_unreachable"),
        run("2026-11-07T00:30:00Z", "succeeded"),
        run("2026-11-06T00:30:00Z", "succeeded", { warnings: ["skipped one file"] }),
      ],
      f,
    );
    expect(rows.map((r) => [r.run.state, r.count, r.expandable])).toEqual([
      ["pbs_unreachable", 3, true],
      ["pbs_unreachable", 1, true], // 23:30 local on the 7th
      ["succeeded", 1, false],
      ["succeeded", 1, true],
    ]);
  });

  it("leaves out active runs", () => {
    expect(historyRows([run("2026-11-08T17:00:00Z", "inventory")], f)).toEqual([]);
  });
});

describe("runStep", () => {
  it("maps run states to the four progress steps", () => {
    expect(["starting", "waking_pbs", "inventory", "uploading", "finishing"].map((s) => runStep(s as Run["state"]))).toEqual([0, 0, 1, 2, 3]);
  });
});

describe("machines", () => {
  const base: Machine = { backup_id: "media", last_snapshot: "2026-11-08T00:50:00Z" };

  it("derives the state of another PC from snapshot and report", () => {
    expect(machineKind(base, now)).toBe("ok");
    expect(machineKind({ ...base, last_snapshot: null }, now)).toBe("never");
    expect(machineKind({ ...base, last_snapshot: "2026-11-05T00:50:00Z" }, now)).toBe("overdue");
    expect(machineKind({ ...base, report: { reported_at: "2026-11-08T00:52:00Z", result: "succeeded", key_exported: false } }, now)).toBe("key_missing");
    const failed = { reported_at: "2026-11-08T05:00:00Z", result: "failed" as const, run_started_at: "2026-11-08T04:59:00Z" };
    expect(machineKind({ ...base, report: failed }, now)).toBe("failing");
    // A failure older than the last snapshot is history, not the current state.
    expect(machineKind({ ...base, report: { ...failed, run_started_at: "2026-11-07T04:59:00Z" } }, now)).toBe("ok");
  });

  it("uses the reported overdue threshold", () => {
    const m = { ...base, last_snapshot: "2026-11-07T10:00:00Z" };
    expect(machineKind(m, now)).toBe("ok");
    expect(machineKind({ ...m, report: { reported_at: m.last_snapshot, result: "succeeded", overdue_after_hours: 6 } }, now)).toBe("overdue");
  });

  it("sorts problems first and leaves out this PC", () => {
    const list = sortMachines(
      [
        { backup_id: "desktop", last_snapshot: "2026-11-08T00:50:00Z" },
        { backup_id: "workshop", last_snapshot: "2026-11-08T00:50:00Z" },
        { backup_id: "laptop", last_snapshot: "2026-10-30T00:50:00Z" },
        { backup_id: "attic", last_snapshot: "2026-11-08T00:50:00Z" },
        { ...base, report: { reported_at: "2026-11-08T05:00:00Z", result: "failed" } },
      ],
      "desktop",
      now,
    );
    expect(list.map((m) => m.backup_id)).toEqual(["media", "laptop", "attic", "workshop"]);
  });
});

describe("dayStrip in other time zones", () => {
  it.each([
    ["Pacific/Auckland", "2026-11-08T13:30:00Z", "2026-11-09", "Mo 9. Nov.", "9"], // Monday 02:30 in Auckland
    ["America/Los_Angeles", "2026-11-09T07:30:00Z", "2026-11-08", "So 8. Nov.", "8"], // Sunday 23:30 in Los Angeles
    ["Europe/Berlin", "2026-11-08T23:30:00Z", "2026-11-09", "Mo 9. Nov.", "9"],
  ])("%s: today is the local calendar day, labelled as such", (tz, nowIso, todayKey, label, num) => {
    const fz = createFormatter("de", "de-DE", tz);
    const cells = dayStrip(runEvents([run(nowIso, "succeeded")]), new Date(nowIso), fz);
    const today = cells[13]!;
    expect(today.key).toBe(todayKey);
    expect(today.kind).toBe("ok");
    expect(fz.keyShort(today.key)).toBe(label);
    expect(fz.keyNumber(today.key)).toBe(num);
    // The labels must agree with how the run itself is shown.
    expect(fz.dayShort(nowIso)).toBe(label);
  });
});

describe("serverContact", () => {
  it("counts only runs with evidence of contact", () => {
    const failed = (kind: NonNullable<Run["error"]>["kind"]) => run("2026-11-08T00:31:00Z", "failed", { error: { kind, message: "" } });
    expect(serverContact(run("x", "succeeded"))).toBe("reached");
    expect(serverContact(run("x", "pbs_unreachable"))).toBe("not_reached");
    expect(serverContact(failed("authentication"))).toBe("reached");
    expect(serverContact(failed("client_failed"))).toBe("reached");
    expect(serverContact(failed("client_missing"))).toBe("reached");
    // Checked before the PBS is contacted, or anywhere: no evidence.
    expect(serverContact(failed("source_missing"))).toBe("unknown");
    expect(serverContact(failed("config"))).toBe("unknown");
    expect(serverContact(failed("already_running"))).toBe("unknown");
    expect(serverContact(failed("internal"))).toBe("unknown");
    expect(serverContact(run("x", "failed"))).toBe("unknown");
    expect(serverContact(run("x", "cancelled"))).toBe("unknown");
  });
});

describe("misc", () => {
  it("reads the interval from the daemon's schedule text", () => {
    expect(intervalHours("every 24 h")).toBe(24);
    expect(intervalHours("every 12 h (checked hourly)")).toBe(12);
    expect(intervalHours("daily")).toBeNull();
    expect(intervalHours(undefined)).toBeNull();
  });

  it("lists restorable archives", () => {
    expect(restorableArchives(["home.pxar.didx", "etc.pxar.didx", "index.json.blob", "client.log.blob"])).toEqual(["home.pxar", "etc.pxar"]);
    expect(restorableArchives(undefined)).toEqual([]);
  });
});
