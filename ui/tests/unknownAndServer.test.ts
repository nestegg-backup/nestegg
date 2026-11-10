// Values written by a newer version (`unknown`), the server line after a connection check,
// automatic backups switched off, the setup guide's pointer to the documentation, and the
// daemon's literal settings messages.
import { describe, expect, it } from "vitest";
import { World } from "../mock/world";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { newestReach, reachOfCheck } from "../src/lib/check";
import {
  dayStrip,
  errorKind,
  historyRows,
  isUnknownRun,
  knownErrorKind,
  machineEvents,
  runEvents,
  runState,
  runStep,
  runTone,
  runTrigger,
  selfKind,
  selfStrip,
  serverContact,
} from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { settingsProblem } from "../src/lib/settings";
import { problemText, saveFailure } from "../src/lib/settingsForm";
import { runResultText, selfRowText, selfSummary, serverRow } from "../src/lib/text";
import { ApiFailure } from "../src/lib/api";
import type { Machine, PbsCheckResult, Run, Status } from "../src/lib/types";

const fde = createFormatter("de", "de-DE", "Europe/Berlin");
const fen = createFormatter("en", "en-GB", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");
const both = [
  ["de", de, fde],
  ["en", en, fen],
] as const;

function run(started_at: string, state: string, over: Partial<Run> = {}): Run {
  return { id: `r-${started_at}-${state}`, trigger: "timer", state: state as Run["state"], started_at, ...over };
}

const ok = run("2026-11-08T00:42:00Z", "succeeded", { snapshot: "host/desktop/2026-11-08T00:42:00Z" });

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true, next_run: "2026-11-09T00:30:00Z", interval_hours: 24 },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: true },
    health: "ok",
    last_run: ok,
    last_success: ok,
    ...over,
  };
}

describe("states, triggers and error kinds of a newer version", () => {
  const future = ["unknown", "paused", "", "SUCCEEDED"];

  it("every word this page does not know counts as unknown, the known ones stay", () => {
    for (const w of future) expect(runState({ state: w as Run["state"] }), w).toBe("unknown");
    for (const w of ["starting", "uploading", "succeeded", "failed", "pbs_unreachable", "cancelled"])
      expect(runState({ state: w as Run["state"] })).toBe(w);
    expect(isUnknownRun(run("2026-11-08T00:42:00Z", "paused"))).toBe(true);
    expect(isUnknownRun(ok)).toBe(false);
    expect(isUnknownRun(null)).toBe(false);
    expect(runTrigger({ trigger: "webhook" as Run["trigger"] })).toBe("unknown");
    expect(runTrigger({ trigger: "cli" })).toBe("cli");
    expect(knownErrorKind("quota")).toBe("unknown");
    expect(knownErrorKind(undefined)).toBe("unknown");
    expect(knownErrorKind("config")).toBe("config");
  });

  it("an unknown run gets its own result text: never cancelled, never backed up", () => {
    for (const [, t] of both) {
      for (const w of future) {
        const text = runResultText(run("2026-11-08T00:42:00Z", w), 1, new Set(), t);
        expect(text, w).toBe(t.runs.unknown);
        expect(text).not.toBe(t.runs.cancelled);
        expect(text).not.toBe(t.runs.succeeded);
      }
      // A running state is no result either; it must not be called "cancelled" or "backed up".
      expect(runResultText(run("2026-11-08T00:42:00Z", "uploading"), 1, new Set(), t)).toBe(t.runs.unknown);
      expect(runResultText(run("2026-11-08T00:42:00Z", "cancelled"), 1, new Set(), t)).toBe(t.runs.cancelled);
    }
    expect(en.runs.unknown).toMatch(/update nestegg/i);
    expect(de.runs.unknown).toMatch(/aktualisieren/i);
  });

  it("a trigger it does not know and an error kind it does not know have plain texts", () => {
    for (const [, t] of both) {
      expect(runResultText(run("2026-11-08T00:42:00Z", "succeeded", { trigger: "webhook" as Run["trigger"] }), 1, new Set(), t)).toBe(
        t.runs.succeededOtherStart,
      );
      expect(runResultText(run("2026-11-08T00:42:00Z", "succeeded", { trigger: "cli" }), 1, new Set(), t)).toBe(t.runs.succeededManual);
      const failed = run("2026-11-08T00:42:00Z", "failed", { error: { kind: "quota" as never, message: "x" } });
      expect(errorKind(failed)).toBe("unknown");
      expect(runResultText(failed, 1, new Set(), t)).toBe(t.runs.failed(t.reason.unknown));
      expect(t.explain[errorKind(failed)!]).toBe(t.explain.unknown);
      expect(t.explain.unknown).toMatch(/nestegg/);
      expect(t.runs.succeededOtherStart.length).toBeGreaterThan(0);
      expect(t.self.runningSince("5 min", "unknown")).not.toBe(t.self.runningSince("5 min", "timer"));
    }
    // an unknown state without any error still explains itself
    expect(errorKind(run("2026-11-08T00:42:00Z", "paused"))).toBe("unknown");
    expect(errorKind(ok)).toBeNull();
  });

  it("the history row is expandable and the dot is neither red nor grey", () => {
    const rows = historyRows([run("2026-11-08T00:42:00Z", "paused")], fde);
    expect(rows[0]!.expandable).toBe(true);
    expect(runTone(rows[0]!.run)).toBe("warn");
    expect(runTone(run("2026-11-08T00:42:00Z", "cancelled"))).toBe("neutral");
  });

  it("the day strip marks the day as 'not understood'; any known result of the day wins over it", () => {
    const u = run("2026-11-08T00:42:00Z", "paused");
    expect(runEvents([u])).toEqual([{ at: u.started_at, kind: "unknown" }]);
    const cell = (runs: Run[]) => selfStrip(runs, null, [], now, fde).at(-1)!.kind;
    expect(cell([u])).toBe("unknown");
    expect(cell([u, run("2026-11-08T01:42:00Z", "succeeded")])).toBe("ok");
    expect(cell([u, run("2026-11-08T01:42:00Z", "failed")])).toBe("fail");
    expect(cell([run("2026-11-08T01:42:00Z", "cancelled")])).toBe("none");
    expect(dayStrip([{ at: u.started_at, kind: "unknown" }], now, fde).at(-1)!.kind).toBe("unknown");
    for (const [, t] of both) expect(t.strip.unknown.length).toBeGreaterThan(0);
    // another PC's report with a result this version does not know
    const m: Machine = {
      backup_id: "x",
      report: { reported_at: "2026-11-08T01:00:00Z", result: "paused" as never },
    };
    expect(machineEvents(m).at(-1)!.kind).toBe("unknown");
  });

  it("serverContact and runStep never read an unknown value as a result", () => {
    expect(serverContact(run("2026-11-08T00:42:00Z", "paused"))).toBe("unknown");
    expect(runStep("paused" as Run["state"])).toBe(4);
    expect(serverContact(run("2026-11-08T00:42:00Z", "failed", { error: { kind: "quota" as never, message: "x" } }))).toBe("unknown");
  });

  it("the headline and the list row say what it is, in both languages", () => {
    const unknown = run("2026-11-08T07:00:00Z", "unknown", { trigger: "unknown" });
    // A daemon that already reports `failing` for it, and an older one that still says `ok`.
    for (const health of ["failing", "ok", "overdue", "incomplete"] as const) {
      const st = status({ health, last_run: unknown });
      expect(selfKind(st), health).toBe("failing");
      for (const [, t, f] of both) {
        const s = selfSummary(st, t, f, now);
        const all = `${s.lead} ${s.text} ${selfRowText(st, t, f, now)}`;
        expect(all, health).toMatch(/nestegg/);
        expect(all).not.toMatch(/abgebrochen|cancelled|fehlgeschlagen|failed|Alles in Ordnung|Everything is fine/i);
        expect(selfRowText(st, t, f, now)).toBe(t.row.unknownRun);
        // still says when the last real backup was
        expect(s.text).toMatch(/\d\d:\d\d/);
      }
    }
    expect(selfSummary(status({ health: "failing", last_run: unknown }), en, fen, now).lead).toBe(
      "The last entry in the history, today at 08:00, is not understood by this version of nestegg.",
    );
    expect(selfSummary(status({ health: "failing", last_run: unknown }), de, fde, now).text).toContain("Aktualisiere nestegg.");
  });

  it("without any backup the attempt is described as not understood, not as 'did not work'", () => {
    const unknown = run("2026-11-08T07:00:00Z", "unknown");
    const st = status({ health: "never", last_run: unknown, last_success: null });
    expect(selfKind(st)).toBe("never");
    for (const [, t, f] of both) {
      const s = selfSummary(st, t, f, now);
      expect(s.text).toContain(t.self.neverUnknown(f.moment(unknown.started_at, now)));
      expect(s.text).not.toContain(t.self.neverAttempt(f.moment(unknown.started_at, now)));
    }
  });

  it("a running backup and a missing key still come first", () => {
    const unknown = run("2026-11-08T07:00:00Z", "unknown");
    expect(selfKind(status({ health: "running", last_run: unknown }))).toBe("running");
    expect(selfKind(status({ last_run: unknown, encryption: { enabled: true, key_exported: false } }))).toBe("key_missing");
  });
});

describe("mock scenarios for the unknown values", () => {
  const params = new URLSearchParams();
  it("unknown_run: health failing, an unknown state, trigger and error kind in the history", () => {
    const w = new World("unknown_run", "full");
    const s = w.handle("GET", "/status", params, null).body as Status;
    expect(s.health).toBe("failing");
    expect(s.last_run?.state).toBe("unknown");
    expect(s.last_run?.trigger).toBe("unknown");
    const runs = (w.handle("GET", "/runs", params, null).body as { runs: Run[] }).runs;
    expect(runs.some((r) => r.trigger === "unknown" && r.state === "succeeded")).toBe(true);
    expect(runs.some((r) => r.state === "failed" && r.error?.kind === "unknown")).toBe(true);
  });
  it("unknown_never: no success, the one attempt is unknown", () => {
    const w = new World("unknown_never", "full");
    const s = w.handle("GET", "/status", params, null).body as Status;
    expect(s.health).toBe("never");
    expect(s.last_run?.state).toBe("unknown");
  });
});

describe("what the server line shows after a connection check", () => {
  const result = (items: [string, "passed" | "failed" | "not_checked"][], checked_at: string): PbsCheckResult => ({
    checked_at,
    checks: items.map(([name, status]) => ({ name, ok: status === "passed", status, detail: "" })),
  });
  const runSaid = { reachable: true, checked_at: "2026-11-08T00:42:00Z" };

  it("a check in which the server did not answer wins over the older run", () => {
    for (const status of ["failed", "not_checked"] as const) {
      const r = reachOfCheck(result([["PBS reachable", status]], "2026-11-08T16:00:00Z"));
      expect(r).toEqual({ reachable: false, checked_at: "2026-11-08T16:00:00Z" });
      expect(newestReach(runSaid, r)).toEqual(r);
    }
    const passed = reachOfCheck(result([["PBS reachable", "passed"]], "2026-11-08T16:00:00Z"));
    expect(newestReach({ reachable: false, checked_at: "2026-11-08T00:42:00Z" }, passed)).toEqual({ reachable: true, checked_at: "2026-11-08T16:00:00Z" });
  });

  it("/status wins again as soon as it reports something newer", () => {
    const check = reachOfCheck(result([["PBS reachable", "failed"]], "2026-11-08T16:00:00Z"));
    const newer = { reachable: true, checked_at: "2026-11-08T17:30:00Z" };
    expect(newestReach(newer, check)).toEqual(newer);
  });

  it("without a check, without the reachability item, or without anything in /status", () => {
    expect(newestReach(runSaid, null)).toEqual(runSaid);
    expect(reachOfCheck(result([["configuration", "failed"]], "2026-11-08T16:00:00Z"))).toBeNull();
    const check = reachOfCheck(result([["PBS reachable", "failed"]], "2026-11-08T16:00:00Z"));
    expect(newestReach({}, check)).toEqual(check);
    expect(newestReach({ reachable: null, checked_at: null }, check)).toEqual(check);
  });

  it("the sidebar line follows: not reachable, with the time of the check", () => {
    const check = reachOfCheck(result([["PBS reachable", "failed"]], "2026-11-08T16:00:00Z"))!;
    expect(serverRow(newestReach(runSaid, check), en, fen, now)).toEqual({ tone: "pbs", text: "Not reachable, checked at 17:00" });
    expect(serverRow(newestReach(runSaid, check), de, fde, now).text).toBe("Nicht erreichbar, geprüft um 17:00");
    expect(serverRow(newestReach(runSaid, null), en, fen, now).text).toBe("Reachable, checked at 01:42");
  });
});

describe("automatic backups switched off", () => {
  const off = (over: Partial<Status> = {}) => status({ schedule: { enabled: false, interval_hours: 24 }, ...over });

  it("the headline starts with the fact, names the last backup and points to the settings", () => {
    const s = selfSummary(off(), en, fen, now);
    expect(s.autoOff).toBe(true);
    expect(s.lead).toBe("Automatic backups are off.");
    expect(s.text).toContain("Last backed up today at 01:42.");
    expect(s.text).toContain("settings");
    expect(`${s.lead} ${s.text}`).not.toMatch(/Everything is fine/);
    const d = selfSummary(off(), de, fde, now);
    expect(d.lead).toBe("Die automatische Sicherung ist aus.");
    expect(d.text).toContain("Zuletzt gesichert heute um 01:42.");
    expect(d.text).toContain("Einstellungen");
    expect(`${d.lead} ${d.text}`).not.toMatch(/Alles in Ordnung/);
  });

  it("keeps the notes about damaged backups and an unreachable server", () => {
    const damaged = selfSummary(off({ server_verify_failed: ["host/desktop/2026-11-01T00:42:00Z"] }), en, fen, now);
    expect(damaged.text).toContain(en.self.okOffDamaged);
    const pbs = run("2026-11-08T10:30:00Z", "pbs_unreachable");
    expect(selfSummary(off({ last_run: pbs }), en, fen, now).text).toContain("could not reach the backup server");
  });

  it("says so above a failed, an unknown and a missing first backup too", () => {
    const failed = run("2026-11-08T10:30:00Z", "failed", { error: { kind: "client_failed", message: "x" } });
    const unknown = run("2026-11-08T10:30:00Z", "paused_by_future");
    const cases: [string, Partial<Status>][] = [
      ["failed", { health: "failing", last_run: failed }],
      ["unknown", { health: "failing", last_run: unknown }],
      ["never", { health: "never", last_run: null, last_success: null }],
    ];
    for (const [name, over] of cases) {
      for (const [, t, f] of both) {
        const s = selfSummary(off(over), t, f, now);
        expect(s.autoOff, name).toBe(true);
        expect(s.text, name).toContain(t.self.scheduleOff);
        // Switched on: no such sentence and no link.
        const on = selfSummary(status(over), t, f, now);
        expect(on.autoOff, name).toBeUndefined();
        expect(on.text, name).not.toContain(t.self.scheduleOff);
      }
    }
  });

  it("the list row says it too; switched on, nothing changes", () => {
    expect(selfRowText(off(), en, fen, now)).toBe("Backed up today 01:42, automatic backups off");
    expect(selfRowText(off(), de, fde, now)).toBe("Heute 01:42 gesichert, automatische Sicherung aus");
    expect(selfRowText(status(), en, fen, now)).toBe("Backed up today 01:42");
    expect(selfSummary(status(), en, fen, now).autoOff).toBeUndefined();
    expect(selfSummary(status(), en, fen, now).text).toMatch(/^Everything is fine\./);
  });
});

describe("setup guide: where the documentation is", () => {
  it("names the installed file and the English section title, not a path of the source tree", () => {
    for (const [, t] of both) {
      expect(t.guide.setupClient).toContain("/usr/share/doc/nestegg/configuration.md");
      expect(t.guide.setupClient).toContain("Installing the backup client");
      expect(t.guide.setupClient).not.toContain("docs/configuration.md");
    }
    expect(de.guide.setupClient).toMatch(/englischen/);
    expect(en.guide.setupClient).toMatch(/in English/);
  });
});

describe("the daemon's literal settings messages", () => {
  // Copied from crates/nestegg/src/settings.rs and from what serde_json says about a body of the
  // wrong type (checked against serde_json 1.0: the message names no field).
  const literal = [
    "interval_hours must be between 1 and 168 (one week)",
    "overdue_after_hours must be between 2 and 168 hours through the web interface. A longer time can only be set as root in /etc/nestegg/config.toml.",
    "overdue_after_hours must be between 2 and 744 hours through the web interface; the current value of 744 can only be lowered here. A longer time can only be set as root in /etc/nestegg/config.toml.",
    'overdue_after_hours (24) must be greater than interval_hours (48); otherwise the dashboard reports "overdue" before the next backup is due',
    "at most 200 exclude patterns are allowed",
    "exclude pattern `a` is listed twice",
    "exclude patterns must not be empty",
    "exclude patterns may have at most 256 bytes",
    'exclude pattern "a\\nb" contains a control character or an invisible format character (line break, tab, NUL, zero-width or bidirectional override, ...)',
    'exclude pattern " a" must not start or end with spaces',
    "exclude pattern `-a` must not start with `-`",
    "exclude pattern `**` would exclude everything; the backups would be empty",
    "exclude pattern `[abc` is not a valid pattern (unclosed character class); the backup client would reject it",
    "exclude pattern `foo\\` is malformed (an unclosed `[`…`]` character class or a dangling `\\`, for example) and would break every backup run",
    "this exclude list would leave out the whole folder /home/alice from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.",
    "Wake-on-LAN is not set up: the configuration has no [wake] section with the MAC address of the backup server. Add it in the terminal (see /usr/share/doc/nestegg/configuration.md), then switch it on here.",
    '"datastore" cannot be changed here; as root, edit /etc/nestegg/config.toml or use the nestegg commands in the terminal',
    // wrong types and numbers out of range (interval_hours / overdue_after_hours)
    "invalid settings: invalid value: integer `-1`, expected u32",
    "invalid settings: invalid type: floating point `1.5`, expected u32",
    'invalid settings: invalid type: string "x", expected u32',
    "invalid settings: invalid value: integer `5000000000`, expected u32",
    "invalid settings: invalid type: boolean `true`, expected u32",
    "invalid settings: invalid type: null, expected u32",
  ];

  it.each(both.map(([lang, t]) => [lang, t] as const))("has a %s text for each of them", (_lang, t) => {
    for (const message of literal) expect(problemText(message, t), message).not.toBeNull();
  });

  it("maps the number problems to the 'whole number' text and nothing else to it", () => {
    for (const [, t] of both) {
      for (const message of literal.filter((m) => m.startsWith("invalid settings:"))) expect(problemText(message, t), message).toBe(t.settings.problem.wholeNumber);
      expect(problemText("invalid settings: invalid type: string \"x\", expected a boolean", t)).toBeNull();
      expect(problemText("invalid settings: invalid type: integer `1`, expected a string", t)).toBeNull();
    }
  });

  it("the stand-in (mock) says exactly what the daemon says for a wrong type", () => {
    const current = { interval_hours: 24, overdue_after_hours: 36, excludes: [], wake_on_lan: true, wake_on_lan_configured: true };
    const say = (body: unknown) => settingsProblem(body, current);
    expect(say({ interval_hours: -1 })).toBe("invalid settings: invalid value: integer `-1`, expected u32");
    expect(say({ interval_hours: 1.5 })).toBe("invalid settings: invalid type: floating point `1.5`, expected u32");
    expect(say({ interval_hours: "x" })).toBe('invalid settings: invalid type: string "x", expected u32');
    expect(say({ overdue_after_hours: 5_000_000_000 })).toBe("invalid settings: invalid value: integer `5000000000`, expected u32");
    expect(say({ interval_hours: true })).toBe("invalid settings: invalid type: boolean `true`, expected u32");
    expect(say({ overdue_after_hours: [1] })).toBe("invalid settings: invalid type: sequence, expected u32");
    expect(say({ wake_on_lan: "yes" })).toBe('invalid settings: invalid type: string "yes", expected a boolean');
    expect(say({ automatic_backups: 1 })).toBe("invalid settings: invalid type: integer `1`, expected a boolean");
    expect(say({ excludes: "x" })).toBe('invalid settings: invalid type: string "x", expected a sequence');
    expect(say({ excludes: { a: 1 } })).toBe("invalid settings: invalid type: map, expected a sequence");
    expect(say({ excludes: ["a", 1] })).toBe("invalid settings: invalid type: integer `1`, expected a string");
    expect(say({ excludes: [null] })).toBe("invalid settings: invalid type: null, expected a string");
    // and the page turns each of the number cases into its own text
    for (const body of [{ interval_hours: -1 }, { interval_hours: 1.5 }, { overdue_after_hours: "x" }]) {
      for (const [, t] of both) expect(problemText(say(body)!, t)).toBe(t.settings.problem.wholeNumber);
    }
  });

  it("a 400 with such a message is shown with the text, not as an unknown problem", () => {
    const f = new ApiFailure("bad_request", 400, "bad_request", "invalid settings: invalid value: integer `-1`, expected u32");
    expect(saveFailure(f, en)).toEqual({ text: en.settings.problem.wholeNumber, detail: null });
  });
});
