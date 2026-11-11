// The backup stick as a second target (decisions 0023 and 0026): truthful states, both targets in
// status and history, stick-only setups that do not look broken, and the settings rules of 0022.
import { afterEach, describe, expect, it, vi } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { STICK_UUID, World } from "../mock/world";
import {
  backupBlocked,
  canBackUpToStick,
  hasPbs,
  historyRows,
  knownStickUuid,
  primaryTarget,
  runEvents,
  serverContact,
  setupGuide,
  stickKeyCheckCommand,
  stickOnly,
  stickView,
  hasPlaceholder,
} from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { settingsProblem } from "../src/lib/settings";
import { problemText } from "../src/lib/settingsForm";
import { explainOf, runResultText, selfRowText, selfSummary, stickLine } from "../src/lib/text";
import { app } from "../src/lib/store.svelte";
import type { ConfigView, Run, Status, TargetStatus } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const fen = createFormatter("en", "en-GB", "Europe/Berlin");
const now = new Date("2026-11-11T18:00:00Z");
const params = new URLSearchParams();

const run = (over: Partial<Run> = {}): Run => ({
  id: "r",
  target: "stick",
  stick_uuid: STICK_UUID,
  trigger: "plug",
  state: "succeeded",
  started_at: "2026-11-10T18:10:00Z",
  finished_at: "2026-11-10T18:20:00Z",
  ...over,
});
const absent = (at: string): Run => ({ id: `a-${at}`, target: "stick", trigger: "timer", state: "stick_not_plugged_in", started_at: at, error: { kind: "stick_not_plugged_in", message: "stick not plugged in" } });

function target(over: Partial<TargetStatus> = {}): TargetStatus {
  return {
    health: "ok",
    state: "ok",
    last_run: run(),
    last_success: run(),
    current_run: null,
    consecutive_failures: 0,
    overdue_after_hours: 168,
    available_bytes: 212e9,
    password_reminder: false,
    ...over,
  };
}

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups", credentials_stored: true },
    schedule: { enabled: true, next_run: "2026-11-12T01:00:00Z" },
    encryption: { enabled: true, key_exported: true, key_present: true },
    health: "ok",
    last_run: run({ target: "pbs", trigger: "timer" }),
    last_success: run({ target: "pbs", trigger: "timer" }),
    ...over,
  };
}

const stickOnlyStatus = (stick: Partial<TargetStatus> = {}, over: Partial<Status> = {}) =>
  status({
    targets: { stick: target(stick) },
    // Like the daemon: no server, so no token secret is stored.
    pbs: { server: "", datastore: "", credentials_stored: false, reachable: null },
    last_run: stick.last_run ?? run(),
    last_success: stick.last_success ?? run(),
    ...over,
  });

describe("which targets there are", () => {
  it("older daemons and an unconfigured one mean the server; only a lone stick has none", () => {
    expect(hasPbs(status())).toBe(true);
    expect(hasPbs(status({ targets: {} }))).toBe(true);
    expect(hasPbs(status({ targets: { pbs: target(), stick: target() } }))).toBe(true);
    expect(stickOnly(stickOnlyStatus())).toBe(true);
    expect(primaryTarget(stickOnlyStatus())).toBe("stick");
    expect(primaryTarget(status({ targets: { pbs: target(), stick: target() } }))).toBe("pbs");
  });
});

describe("stick states", () => {
  it("not plugged in is its own state, not a failure, and offers no backup", () => {
    const v = stickView(target({ state: "stick_not_plugged_in", last_run: absent("2026-11-11T17:00:00Z") }));
    expect(v.plugged).toBe(false);
    expect(v.kind).toBe("ok");
    expect(v.safeToUnplug).toBe(false);
    expect(canBackUpToStick(v, false)).toBe(false);
  });

  it("safe to unplug only after a successful stick run, while plugged in and idle", () => {
    expect(stickView(target()).safeToUnplug).toBe(true);
    const failed = run({ state: "failed", error: { kind: "client_failed", message: "x" } });
    expect(stickView(target({ health: "failing", state: "failing", last_run: failed })).safeToUnplug).toBe(false);
    const current = run({ id: "now", state: "uploading", finished_at: null });
    const running = stickView(target({ health: "running", state: "running", current_run: current }));
    expect(running.kind).toBe("running");
    expect(running.safeToUnplug).toBe(false);
    expect(canBackUpToStick(running, true)).toBe(false);
  });

  it("names full, damaged, verification, key and IPv6 problems by their error kind", () => {
    const failing = (kind: NonNullable<Run["error"]>["kind"]) =>
      stickView(target({ health: "failing", state: kind === "stick_full" ? "stick_full" : "failing", last_run: run({ state: "failed", error: { kind, message: "m" } }) })).kind;
    expect(failing("stick_full")).toBe("full");
    expect(failing("filesystem_damaged")).toBe("damaged");
    expect(failing("verification_failed")).toBe("verify_failed");
    expect(failing("key_mismatch")).toBe("key_mismatch");
    expect(failing("stick_unsupported")).toBe("unsupported");
    expect(failing("client_failed")).toBe("failing");
    // `stick_full` hides whether it is plugged in: never claim either.
    expect(stickView(target({ state: "stick_full", health: "failing", last_run: run({ state: "failed", error: { kind: "stick_full", message: "m" } }) })).plugged).toBeNull();
  });

  it("a full stick can be tried again when plugged in", () => {
    const v = stickView(target({ state: "stick_full", health: "failing", last_run: run({ state: "failed", error: { kind: "stick_full", message: "m" } }) }));
    expect(canBackUpToStick(v, false)).toBe(true);
  });

  it("the row says overdue, not plugged in, and how to back up", () => {
    const line = stickLine(
      target({ health: "overdue", state: "stick_not_plugged_in", last_run: absent("2026-11-11T17:00:00Z"), last_success: run({ started_at: "2026-11-02T18:00:00Z", finished_at: "2026-11-02T18:10:00Z" }) }),
      de,
      f,
      now,
    );
    expect(line.lead).toBe("Seit 8 Tagen nicht auf den Stick gesichert.");
    expect(line.plug).toContain(de.stick.absent);
    expect(line.plug).toContain(de.stick.plugToBackUp);
    expect(stickLine(target(), en, fen, now).plug).toBe(en.stick.safe);
  });

  it("explains a failed check on the stick without blaming a server", () => {
    const r = run({ state: "failed", error: { kind: "verification_failed", message: "m" } });
    expect(explainOf(r, de)).toBe(de.stick.verifyText);
    expect(explainOf({ ...r, target: "pbs" }, de)).toBe(de.explain.verification_failed);
    for (const t of [de, en]) {
      for (const k of ["stick_full", "filesystem_damaged", "stick_unsupported", "key_mismatch", "stick_not_plugged_in"] as const) {
        expect(t.reason[k]).toBeTruthy();
        expect(t.explain[k]).toBeTruthy();
      }
    }
    expect(en.explain.stick_unsupported).toMatch(/IPv6/);
  });
});

describe("history of the stick", () => {
  it("folds the hourly 'not plugged in' records of a day and draws no mark for them", () => {
    const runs = [absent("2026-11-11T17:00:00Z"), absent("2026-11-11T16:00:00Z"), absent("2026-11-11T15:00:00Z"), run()];
    const rows = historyRows(runs, f);
    expect(rows).toHaveLength(2);
    expect(rows[0]!.count).toBe(3);
    expect(runResultText(rows[0]!.run, 3, new Set(), de)).toBe("Stick nicht eingesteckt, 3-mal nachgesehen");
    expect(runResultText(run(), 1, new Set(), en)).toBe(en.runs.succeededPlug);
    expect(runEvents(runs).map((e) => e.kind)).toEqual(["ok"]);
  });

  it("a stick run says nothing about the server's reachability", () => {
    expect(serverContact(run())).toBe("unknown");
  });

  it("finds the stick's UUID for the commands, and the command cannot be copied without one", () => {
    expect(knownStickUuid([absent("2026-11-11T17:00:00Z"), run()], null)).toBe(STICK_UUID);
    expect(hasPlaceholder(stickKeyCheckCommand(""))).toBe(true);
    expect(stickKeyCheckCommand(STICK_UUID)).toBe(`sudo nestegg stick key-check --uuid=${STICK_UUID}`);
  });
});

describe("stick-only setups do not look broken", () => {
  it("no token-secret step and no 'token secret missing' block without a server", () => {
    const s = stickOnlyStatus({}, { health: "never", last_run: null, last_success: null });
    expect(setupGuide(s)?.current).toBe("first_backup");
    expect(setupGuide(s)?.steps).not.toContain("credentials");
    expect(backupBlocked(stickOnlyStatus())).toBeNull();
    // With a server the step stays.
    expect(backupBlocked(status({ pbs: { server: "x", datastore: "y", credentials_stored: false } }))).toBe("credentials");
  });

  it("waits for the stick instead of promising a time when it is not plugged in", () => {
    const s = stickOnlyStatus({ state: "stick_not_plugged_in", last_run: absent("2026-11-11T17:00:00Z") }, { last_run: absent("2026-11-11T17:00:00Z") });
    const text = selfSummary(s, de, f, now).text;
    expect(text).toContain(de.stick.waitPlug);
    expect(text).not.toMatch(/nächste Sicherung ist/);
  });
});

describe("server and stick together", () => {
  const overdueStick = target({
    health: "overdue",
    state: "stick_not_plugged_in",
    last_run: absent("2026-11-11T17:00:00Z"),
    last_success: run({ started_at: "2026-11-01T18:00:00Z", finished_at: "2026-11-01T18:10:00Z" }),
  });

  it("the sidebar names the stick's problem when the server is fine", () => {
    const s = status({ targets: { pbs: target({ last_run: run({ target: "pbs" }) }), stick: overdueStick } });
    expect(selfRowText(s, de, f, now)).toBe("Stick überfällig, zuletzt 1. Nov.");
    expect(selfRowText(s, de, f, now, null, false)).not.toMatch(/Stick/);
    // A server problem comes first.
    const failing = status({
      health: "failing",
      last_run: run({ target: "pbs", state: "failed", error: { kind: "client_failed", message: "x" } }),
      targets: { pbs: target(), stick: overdueStick },
    });
    expect(selfRowText(failing, de, f, now)).not.toMatch(/Stick/);
  });

  it("does not say 'everything is fine' while the stick has a problem", () => {
    const s = status({ targets: { pbs: target(), stick: overdueStick } });
    const text = selfSummary(s, en, fen, now).text;
    expect(text).not.toContain(en.self.okText);
    expect(text).toContain(en.stick.okServerOnly);
  });
});

describe("settings of the stick (decisions 0022 and 0026)", () => {
  const current = { interval_hours: 24, overdue_after_hours: 36, wake_on_lan_configured: true, stick_overdue_after_hours: 168, targets: { pbs: true, stick: true } };

  it("the stick threshold has the same cap as the server's", () => {
    expect(settingsProblem({ stick_overdue_after_hours: 168 }, current)).toBeNull();
    const refused = settingsProblem({ stick_overdue_after_hours: 200 }, current)!;
    expect(refused).toBe("stick_overdue_after_hours must be between 1 and 168");
    expect(problemText(refused, de)).toBe(de.settings.problem.stickOverdueRange(168));
    // A weekly rhythm allows ten days, and a larger value root set in the file can only be lowered.
    expect(settingsProblem({ interval_hours: 168, overdue_after_hours: 240, stick_overdue_after_hours: 240 }, current)).toBeNull();
    expect(settingsProblem({ stick_overdue_after_hours: 300 }, { ...current, stick_overdue_after_hours: 300 })).toBeNull();
    expect(settingsProblem({ stick_overdue_after_hours: 0 }, current)).toMatch(/between 1 and/);
    expect(settingsProblem({ stick_overdue_after_hours: 100 }, { ...current, targets: { pbs: true, stick: false } })).toBe("no stick target configured");
  });

  it("the last target cannot be removed", () => {
    expect(settingsProblem({ remove_target: "pbs" }, current)).toBeNull();
    const last = settingsProblem({ remove_target: "stick" }, { ...current, targets: { pbs: false, stick: true } })!;
    expect(problemText(last, en)).toBe(en.settings.problem.lastTarget);
    expect(settingsProblem({ remove_target: "nas" }, current)).toMatch(/unknown variant/);
  });
});

describe("mock: per-target endpoints like the daemon", () => {
  const st = (w: World, t?: number) => w.handle("GET", "/status", params, null, t).body as Status;
  const runsOf = (w: World, target: string, t?: number) => (w.handle("GET", `/targets/${target}/runs`, params, null, t).body as { runs: Run[] }).runs;

  it("stick only: targets has the stick alone, the server endpoints answer as without a server", () => {
    const w = new World("stick_only", "full");
    const s = st(w);
    expect(Object.keys(s.targets ?? {})).toEqual(["stick"]);
    expect(s.pbs.credentials_stored).toBe(false);
    expect(w.handle("GET", "/snapshots", params, null).status).toBe(503);
    const c = w.handle("GET", "/config", params, null).body as ConfigView;
    expect(c.editable).toContain("stick_overdue_after_hours");
    expect(c.editable).not.toContain("remove_target");
    expect(c.editable).not.toContain("wake_on_lan");
    expect(w.handle("POST", "/targets/pbs/runs", params, null).status).toBe(400);
  });

  it("'back up to stick now' runs, then the stick is safe to unplug and has less space", () => {
    const t0 = Date.now();
    const w = new World("both", "full", t0);
    const before = st(w, t0).targets!.stick!.available_bytes!;
    expect(w.handle("POST", "/targets/stick/runs", params, null, t0).status).toBe(202);
    expect(w.handle("POST", "/targets/stick/runs", params, null, t0 + 100).status).toBe(409);
    expect(st(w, t0 + 6_000).targets!.stick!.current_run?.state).toBe("uploading");
    expect(st(w, t0 + 6_000).targets!.pbs!.current_run).toBeNull();
    const after = st(w, t0 + 25_000).targets!.stick!;
    expect(stickView(after).safeToUnplug).toBe(true);
    expect(after.available_bytes!).toBeLessThan(before);
    expect(runsOf(w, "stick", t0 + 25_000)[0]!.state).toBe("succeeded");
    expect(runsOf(w, "pbs", t0 + 25_000).every((r) => r.target !== "stick")).toBe(true);
  });

  it("'back up now' with both: an absent stick is recorded as such, then the server backs up", () => {
    const t0 = Date.now();
    const w = new World("both_absent", "full", t0);
    expect((w.handle("POST", "/runs", params, null, t0).body as Run).target).toBe("stick");
    expect(st(w, t0 + 2_000).current_run?.target).toBe("pbs");
    expect(runsOf(w, "stick", t0 + 2_000)[0]!.state).toBe("stick_not_plugged_in");
    expect(st(w, t0 + 30_000).targets!.pbs!.last_run?.state).toBe("succeeded");
  });

  it("removing a target changes the configuration and the targets", () => {
    const w = new World("both", "full");
    expect(w.handle("PATCH", "/config", params, { remove_target: "pbs" }).status).toBe(200);
    expect(Object.keys(st(w).targets ?? {})).toEqual(["stick"]);
    expect(w.handle("PATCH", "/config", params, { remove_target: "stick" }).status).toBe(400);
  });

  it("the password reminder follows the fault", () => {
    const w = new World("both", "full");
    expect(st(w).targets!.stick!.password_reminder).toBe(false);
    w.faults.add("stick_password");
    expect(st(w).targets!.stick!.password_reminder).toBe(true);
  });
});

describe("store", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    app.status = null;
  });

  function serve(w: World) {
    const paths: string[] = [];
    vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
      const u = new URL(url, "http://localhost/");
      const path = u.pathname.replace(/^\/api\/v1/, "");
      paths.push(`${init?.method ?? "GET"} ${path}`);
      const r = w.handle(init?.method ?? "GET", path, u.searchParams, init?.body ? JSON.parse(String(init.body)) : null);
      return new Response(JSON.stringify(r.body), { status: r.status });
    });
    return paths;
  }

  it("loads one history per target and starts a stick backup on the stick target", async () => {
    const w = new World("both", "full");
    const paths = serve(w);
    await app.poll();
    await app.loadRuns();
    expect(paths).toContain("GET /targets/pbs/runs");
    expect(paths).toContain("GET /targets/stick/runs");
    expect(app.runsOf("stick").value.every((r) => r.target === "stick")).toBe(true);
    expect(app.runsOf("pbs").value.every((r) => r.target === "pbs")).toBe(true);
    await app.startStickRun();
    expect(paths).toContain("POST /targets/stick/runs");
    expect(app.status?.current_run?.target).toBe("stick");
    // A second start is refused by the daemon and says so, without claiming anything started.
    await app.startStickRun();
    expect(app.stickStartError?.kind).toBe("conflict");
  });

  it("stick only: the history of this PC is the stick's", async () => {
    const w = new World("stick_only", "full");
    const paths = serve(w);
    await app.poll();
    await app.loadRuns();
    expect(paths).toContain("GET /targets/stick/runs");
    expect(paths).not.toContain("GET /targets/pbs/runs");
    expect(app.runs.value[0]?.target).toBe("stick");
  });
});
