// The states added with the incomplete / verified / damaged work: derivation and wording.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import {
  damagedSnapshots,
  damagedTimes,
  dayStrip,
  errorKind,
  historyRows,
  isDamaged,
  isIncomplete,
  otherWarnings,
  runEvents,
  scheduleHours,
  selfKind,
  selfStrip,
  selfTone,
  snapshotTimeOf,
} from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { selfRowText, selfSummary } from "../src/lib/text";
import type { Run, Snapshot, Status } from "../src/lib/types";

const fde = createFormatter("de", "de-DE", "Europe/Berlin");
const fen = createFormatter("en", "en-GB", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");

const reasons = [
  "full system backup skipped: the root file system (zfs) cannot be recreated by the rescue stick; only the configured folders are backed up",
  "optional source /mnt/data does not exist, skipped",
];

function run(started_at: string, state: Run["state"], over: Partial<Run> = {}): Run {
  return { id: `r-${started_at}-${state}`, trigger: "timer", state, started_at, ...over };
}

const incompleteRun = run("2026-11-08T00:42:00Z", "succeeded", {
  snapshot: "host/desktop/2026-11-08T00:42:00Z",
  incomplete: reasons,
  warnings: reasons,
});

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true, next_run: "2026-11-09T00:30:00Z", interval_hours: 24 },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: true },
    health: "incomplete",
    last_run: incompleteRun,
    last_success: incompleteRun,
    ...over,
  };
}

describe("incomplete backups", () => {
  it("health=incomplete is its own state in the attention colour, not a failure", () => {
    expect(selfKind(status())).toBe("incomplete");
    expect(selfTone("incomplete")).toBe("warn");
  });

  it("failing and overdue stay above it (the API resolves that, the page follows)", () => {
    expect(selfKind(status({ health: "failing" }))).toBe("failing");
    expect(selfKind(status({ health: "overdue" }))).toBe("overdue");
  });

  it("a missing key copy and a running backup come first, as for every other state", () => {
    expect(selfKind(status({ encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: false } }))).toBe("key_missing");
    expect(selfKind(status({ health: "running" }))).toBe("running");
  });

  it("says 'backed up, but incomplete' in the sentence and in the list row", () => {
    const s = selfSummary(status(), de, fde, now);
    expect(s.lead).toBe("Heute um 01:42 gesichert, aber nicht vollständig.");
    expect(s.text).toContain("Was fehlt, steht unten.");
    expect(selfRowText(status(), de, fde, now)).toBe("Heute 01:42 gesichert, unvollständig");
    expect(selfSummary(status(), en, fen, now).lead).toBe("Backed up today at 01:42, but not completely.");
    expect(selfRowText(status(), en, fen, now)).toBe("Backed up today 01:42, incomplete");
  });

  it("works without run details (allowed by the contract)", () => {
    const bare = status({ last_run: null, last_success: null });
    expect(selfSummary(bare, de, fde, now).lead).toBe("Gesichert, aber nicht vollständig.");
    expect(selfRowText(bare, en, fen, now)).toBe("Backed up, incomplete");
  });

  it("a run with reasons is incomplete only when it succeeded", () => {
    expect(isIncomplete(incompleteRun)).toBe(true);
    expect(isIncomplete({ ...incompleteRun, incomplete: [] })).toBe(false);
    expect(isIncomplete(run("2026-11-08T00:42:00Z", "succeeded"))).toBe(false);
    expect(isIncomplete({ ...incompleteRun, state: "failed" })).toBe(false);
  });

  it("does not list a reason twice: the warnings that are not reasons stay", () => {
    expect(otherWarnings({ ...incompleteRun, warnings: [...reasons, "/home/alice/.cache: permission denied"] })).toEqual([
      "/home/alice/.cache: permission denied",
    ]);
    expect(otherWarnings(incompleteRun)).toEqual([]);
  });

  it("history rows of an incomplete run can be opened to read the reasons", () => {
    const rows = historyRows([incompleteRun, run("2026-11-07T00:42:00Z", "succeeded")], fde);
    expect(rows.map((r) => r.expandable)).toEqual([true, false]);
  });

  it("the 14-day strip marks the day, and a complete backup of the same day wins", () => {
    const runs = [
      incompleteRun,
      run("2026-11-07T00:42:00Z", "succeeded", { incomplete: reasons }),
      run("2026-11-07T12:00:00Z", "succeeded"),
    ];
    const by = Object.fromEntries(dayStrip(runEvents(runs), now, fde).map((c) => [c.key, c.kind]));
    expect(by["2026-11-08"]).toBe("incomplete");
    expect(by["2026-11-07"]).toBe("ok");
  });

  it("the server's snapshot list does not turn an incomplete day back into a plain backed-up day", () => {
    const snapshots: Snapshot[] = [
      { time: "2026-11-08T00:42:00Z", snapshot: "host/desktop/2026-11-08T00:42:00Z" },
      { time: "2026-11-05T00:42:00Z", snapshot: "host/desktop/2026-11-05T00:42:00Z" }, // older installation, no local run
    ];
    const by = Object.fromEntries(selfStrip([incompleteRun], null, snapshots, now, fde).map((c) => [c.key, c.kind]));
    expect(by["2026-11-08"]).toBe("incomplete");
    expect(by["2026-11-05"]).toBe("ok");
  });
});

describe("damaged snapshots", () => {
  const verifyFailed = ["host/desktop/2026-11-05T00:42:00Z"];

  it("takes the server's list and the verification state of the snapshots", () => {
    const list: Snapshot[] = [
      { time: "2026-11-06T00:42:00Z", snapshot: "host/desktop/2026-11-06T00:42:00Z", verified: "failed" },
      { time: "2026-11-07T00:42:00Z", snapshot: "host/desktop/2026-11-07T00:42:00Z", verified: "ok" },
    ];
    expect([...damagedSnapshots(list, verifyFailed)].sort()).toEqual([
      "host/desktop/2026-11-05T00:42:00Z",
      "host/desktop/2026-11-06T00:42:00Z",
    ]);
    expect(isDamaged(list[0]!)).toBe(true);
    expect(isDamaged(list[1]!)).toBe(false);
    expect(isDamaged({ snapshot: verifyFailed[0] }, verifyFailed)).toBe(true);
    expect(isDamaged({ snapshot: "host/desktop/other" }, verifyFailed)).toBe(false);
  });

  it("names the days, newest first, and ignores nonsense in the list", () => {
    expect(snapshotTimeOf("host/desktop/2026-11-08T21:30:12Z")).toBe("2026-11-08T21:30:12Z");
    expect(damagedTimes(["host/desktop/2026-10-31T01:46:00Z", "host/desktop/2026-11-05T00:42:00Z", "garbage"])).toEqual([
      "2026-11-05T00:42:00Z",
      "2026-10-31T01:46:00Z",
    ]);
    expect(damagedTimes(undefined)).toEqual([]);
    expect(damagedTimes([])).toEqual([]);
  });

  it("a day whose only snapshot is damaged is not a backed-up day", () => {
    const ok = run("2026-11-05T00:42:00Z", "succeeded", { snapshot: verifyFailed[0] });
    const by = Object.fromEntries(selfStrip([ok], null, [], now, fde, verifyFailed).map((c) => [c.key, c.kind]));
    expect(by["2026-11-05"]).toBe("damaged");
    // Without a local run the server's list carries the same information.
    const fromServer = selfStrip([], null, [{ time: "2026-11-05T00:42:00Z", snapshot: verifyFailed[0], verified: "failed" }], now, fde);
    expect(Object.fromEntries(fromServer.map((c) => [c.key, c.kind]))["2026-11-05"]).toBe("damaged");
  });

  it("another backup on the same day keeps the day backed up", () => {
    const damaged = run("2026-11-05T00:42:00Z", "succeeded", { snapshot: verifyFailed[0] });
    const fine = run("2026-11-05T13:00:00Z", "succeeded", { snapshot: "host/desktop/2026-11-05T13:00:00Z" });
    const by = Object.fromEntries(selfStrip([damaged, fine], null, [], now, fde, verifyFailed).map((c) => [c.key, c.kind]));
    expect(by["2026-11-05"]).toBe("ok");
  });
});

describe("verification_failed and cancelled runs", () => {
  it("have their own plain explanation in both languages", () => {
    const failed = run("2026-11-08T00:31:00Z", "failed", { error: { kind: "verification_failed", message: "read back failed" } });
    expect(errorKind(failed)).toBe("verification_failed");
    for (const msgs of [de, en]) {
      expect(msgs.explain.verification_failed.length).toBeGreaterThan(60);
      expect(msgs.reason.verification_failed.length).toBeGreaterThan(5);
    }
    expect(de.explain.verification_failed).toContain("Starte die Sicherung noch einmal");
    expect(en.explain.verification_failed).toContain("Start the backup again");
    const s = selfSummary(
      status({ health: "failing", last_run: failed, last_success: run("2026-11-07T00:45:00Z", "succeeded") }),
      de,
      fde,
      now,
    );
    expect(s.lead).toBe("Die Sicherung heute um 01:31 ist fehlgeschlagen.");
    expect(s.text).toContain("Gegenlesen");
  });

  it("a cancelled run after the last success is mentioned, one before it is not", () => {
    const success = run("2026-11-07T00:45:00Z", "succeeded");
    const cancelled = run("2026-11-08T10:00:00Z", "cancelled", { error: { kind: "cancelled", message: "cancelled" } });
    const withCancel = status({ health: "ok", last_run: cancelled, last_success: success });
    expect(selfSummary(withCancel, de, fde, now).text).toContain("Die letzte Sicherung wurde heute um 11:00 abgebrochen.");
    expect(selfSummary(withCancel, en, fen, now).text).toContain("The last backup was cancelled today at 11:00.");
    const older = status({ health: "ok", last_run: success, last_success: success });
    expect(selfSummary(older, de, fde, now).text).not.toContain("abgebrochen");
    // Never backed up: the attempt was cancelled, it did not "not work".
    const never = status({ health: "never", last_run: cancelled, last_success: null });
    expect(selfSummary(never, de, fde, now).text).toContain("wurde abgebrochen");
    expect(selfSummary(never, de, fde, now).text).not.toContain("hat nicht geklappt");
  });
});

describe("interval_hours", () => {
  it("comes from the configuration, then from /status, and only then from the text", () => {
    expect(scheduleHours({ interval_hours: 12, schedule: "every 24 h" }, status({ schedule: { enabled: true, interval_hours: 6 } }))).toBe(12);
    expect(scheduleHours(null, status({ schedule: { enabled: true, interval_hours: 6, calendar: "every 24 h" } }))).toBe(6);
    expect(scheduleHours(null, status({ schedule: { enabled: true, calendar: "every 8 h (checked hourly)" } }))).toBe(8);
    expect(scheduleHours(null, status({ schedule: { enabled: true, calendar: "daily" } }))).toBeNull();
    expect(scheduleHours(undefined, null)).toBeNull();
  });
});
