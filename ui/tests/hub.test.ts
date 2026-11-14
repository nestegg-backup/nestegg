// The hub page (decision 0034): state of each computer, the overview sentence, the code
// validity, the API client's rules (header, 401) and the development mock.
import { describe, expect, it, vi } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { createFormatter } from "../src/lib/format";
import { machineSummary } from "../src/lib/text";
import { HUB_HEADER, HubFailure, hubFailureText, hubRequest, setUnauthorizedHandler } from "../src/hub/api";
import {
  asMachine,
  hubKind,
  hubSummary,
  minutesLeft,
  newMachines,
  overview,
  packageCommand,
  sortHubMachines,
  validityText,
} from "../src/hub/derive";
import type { HubMachine, HubMachines } from "../src/hub/types";
import { HUB_PASSWORD, HUB_SCENARIOS, HubWorld, SESSION_COOKIE, hubWorldFor } from "../mock/hub";

const f = createFormatter("en", "en-GB", "Europe/Berlin");
const now = new Date("2026-11-14T12:00:00Z");
const H = 3_600_000;
const ago = (h: number) => new Date(now.getTime() - h * H).toISOString();
const reachable = { pbs_reachable: true, pbs_read_at: ago(0) };

function machine(over: Partial<HubMachine> = {}): HubMachine {
  return {
    machine_id: "m1",
    backup_id: "desktop",
    hostname: "desktop",
    joined_at: ago(24 * 30),
    snapshot_count: 10,
    last_snapshot: ago(10),
    last_snapshot_verified: true,
    report: { reported_at: ago(9.5), result: "succeeded", run_started_at: ago(10), key_exported: false, overdue_after_hours: 36 },
    ...over,
  };
}

describe("hub: state of a computer", () => {
  it("builds on the local interpretation; an own key copy is not asked of a hub computer", () => {
    const m = machine();
    expect(hubKind(m, reachable, now)).toBe("ok");
    // The same report on the local page of another computer would warn about the key copy.
    expect(asMachine(m).report?.key_exported).toBeNull();
    const s = hubSummary(m, reachable, en, f, now);
    expect(s.lead).toBe(machineSummary(asMachine(m), en, f, now).lead);
    expect(s.text).toBe(en.other.okText);
  });

  it("failing and overdue use the local sentences", () => {
    const failing = machine({
      report: { reported_at: ago(1), result: "failed", run_started_at: ago(1.1), error: { kind: "source_missing", message: "x" } },
    });
    expect(hubKind(failing, reachable, now)).toBe("failing");
    expect(hubSummary(failing, reachable, de, f, now)).toMatchObject({ tone: "bad", lead: machineSummary(asMachine(failing), de, f, now).lead });
    const overdue = machine({ last_snapshot: ago(80), report: { ...machine().report!, run_started_at: ago(80), reported_at: ago(79) } });
    expect(hubKind(overdue, reachable, now)).toBe("overdue");
    expect(hubSummary(overdue, reachable, en, f, now).lead).toBe(en.other.overdueLead(3, 80));
  });

  it("damaged, incomplete and the first backup are states of their own", () => {
    expect(hubKind(machine({ last_snapshot_verified: false }), reachable, now)).toBe("damaged");
    const inc = machine({ report: { ...machine().report!, incomplete: ["optional source /mnt/x is not mounted"] } });
    expect(hubKind(inc, reachable, now)).toBe("incomplete");
    const first = machine({ snapshot_count: 0, last_snapshot: null, last_snapshot_verified: null, report: null });
    expect(hubKind(first, reachable, now)).toBe("first_backup");
    expect(hubSummary(first, reachable, de, f, now).lead).toBe(de.hub.state.firstLead);
  });

  it("never claims anything when the PBS was never read", () => {
    const data = { pbs_reachable: false, pbs_read_at: null };
    const first = machine({ snapshot_count: 0, last_snapshot: null, report: null });
    expect(hubKind(first, data, now)).toBe("unknown");
    expect(hubSummary(first, data, en, f, now).lead).toBe(en.hub.state.unknownLead);
  });

  it("with a sleeping PBS a computer is judged at the time the data was read", () => {
    // Backed up 10 h before the read, read 40 h ago: "overdue" against now would blame the computer.
    const m = machine({ last_snapshot: ago(50), report: { ...machine().report!, run_started_at: ago(50), reported_at: ago(49) } });
    const stale = { pbs_reachable: false, pbs_read_at: ago(40) };
    expect(hubKind(m, reachable, now)).toBe("overdue");
    expect(hubKind(m, stale, now)).toBe("ok");
    expect(hubSummary(m, stale, en, f, now).text).toBe(en.hub.state.okStale);
  });

  it("problems first, then by name", () => {
    const list = [
      machine({ machine_id: "a", hostname: "zeta" }),
      machine({ machine_id: "b", hostname: "alpha", last_snapshot_verified: false }),
      machine({ machine_id: "c", hostname: "beta", last_snapshot: null, snapshot_count: 0, report: null }),
    ];
    expect(sortHubMachines(list, reachable, now).map((m) => m.hostname)).toEqual(["alpha", "beta", "zeta"]);
  });
});

describe("hub: overview sentence", () => {
  const data = (machines: HubMachine[], read: string | null = ago(0)): HubMachines => ({ pbs_reachable: !!read, pbs_read_at: read, machines, unmanaged: [] });

  it("empty, fine, waiting, problems, unknown", () => {
    expect(overview(data([]), de, now).lead).toBe(de.hub.summary.emptyLead);
    expect(overview(data([machine()]), en, now)).toMatchObject({ tone: "ok", text: en.hub.summary.okText(1) });
    const first = machine({ machine_id: "x", last_snapshot: null, snapshot_count: 0, report: null });
    expect(overview(data([machine(), first]), en, now).text).toBe(en.hub.summary.waitingText(2, 1));
    expect(overview(data([machine(), machine({ machine_id: "y", last_snapshot_verified: false })]), de, now)).toMatchObject({
      tone: "bad",
      lead: "1 von 2 Computern braucht Aufmerksamkeit.",
    });
    expect(overview(data([machine()], null), en, now).lead).toBe(en.hub.summary.unknownLead);
  });
});

describe("hub: one-time codes", () => {
  it("counts whole minutes up and says when a code has expired", () => {
    const at = (min: number) => new Date(now.getTime() + min * 60_000).toISOString();
    expect(minutesLeft(at(29.2), now)).toBe(30);
    expect(minutesLeft(at(-1), now)).toBe(0);
    expect(validityText(at(30), en, f, now)).toBe(en.hub.dialog.valid(f.time(at(30)), 30));
    expect(validityText(at(0.5), en, f, now)).toBe(en.hub.dialog.validLast(f.time(at(0.5))));
    expect(validityText(at(-2), de, f, now)).toBe(de.hub.dialog.expired(f.time(at(-2))));
  });

  it("finds the computer that joined", () => {
    const before = new Set(["m1"]);
    expect(newMachines(before, [machine(), machine({ machine_id: "m2", hostname: "laptop" })]).map((m) => m.hostname)).toEqual(["laptop"]);
  });

  it("the package command has a placeholder, so it cannot be copied unfinished", () => {
    expect(packageCommand(de)).toBe("sudo nestegg hub package add <DATEI>");
    expect(packageCommand(en)).toBe("sudo nestegg hub package add <FILE>");
  });
});

describe("hub: API client", () => {
  const ok = (status = 200, body = "{}") => new Response(status === 204 ? null : body, { status });

  it("sends X-Nestegg-Hub on every request that changes something, never on GET", async () => {
    const fetchFn = vi.fn(async (_url: string | URL | Request, _init?: RequestInit) => ok());
    await hubRequest("machines", {}, fetchFn as typeof fetch);
    await hubRequest("join-codes", { method: "POST", body: "{}" }, fetchFn as typeof fetch);
    await hubRequest("machines/x", { method: "DELETE" }, fetchFn as typeof fetch);
    const headers = fetchFn.mock.calls.map((c) => (c[1]!.headers as Record<string, string>)[HUB_HEADER]);
    expect(headers).toEqual([undefined, "1", "1"]);
    expect(fetchFn.mock.calls[0]![0]).toBe("api/hub/v1/machines");
    expect(fetchFn.mock.calls[0]![1]!.credentials).toBe("same-origin");
  });

  it("a 401 ends the session, except for the login itself", async () => {
    const ended = vi.fn();
    setUnauthorizedHandler(ended);
    const fetchFn = async () => ok(401, JSON.stringify({ code: "unauthorized", message: "no" }));
    await expect(hubRequest("machines", {}, fetchFn as typeof fetch)).rejects.toMatchObject({ status: 401, code: "unauthorized" });
    expect(ended).toHaveBeenCalledTimes(1);
    await expect(hubRequest("session", { method: "POST", body: "{}" }, fetchFn as typeof fetch)).rejects.toBeInstanceOf(HubFailure);
    expect(ended).toHaveBeenCalledTimes(1);
    setUnauthorizedHandler(() => {});
  });

  it("no answer at all is status 0; 204 resolves", async () => {
    const down = async () => {
      throw new TypeError("Failed to fetch");
    };
    await expect(hubRequest("hub", {}, down as typeof fetch)).rejects.toMatchObject({ status: 0 });
    await expect(hubRequest("session", { method: "DELETE" }, (async () => ok(204)) as typeof fetch)).resolves.toBeUndefined();
  });
});

describe("hub mock", () => {
  const cookieOf = (set: string | undefined) => set!.split(";")[0]!;
  const loggedIn = (w: HubWorld) => `${SESSION_COOKIE}=${w.openSession()}`;
  const get = (w: HubWorld, path: string, cookie?: string) => w.handle("GET", path, null, { cookie });

  it.each(HUB_SCENARIOS.filter((s) => s !== "logged_out" && s !== "rate_limited"))("scenario %s answers the contract", (sc) => {
    const w = new HubWorld(sc);
    const c = loggedIn(w);
    const hub = get(w, "/hub", c);
    expect(hub.status).toBe(200);
    const m = get(w, "/machines", c);
    expect(m.status).toBe(200);
    const body = m.body as HubMachines;
    expect(Array.isArray(body.machines) && Array.isArray(body.unmanaged)).toBe(true);
    expect(get(w, "/join-codes", c).status).toBe(200);
  });

  it("logs in with the password, slows down wrong ones, and needs the header for changes", () => {
    const w = new HubWorld("logged_out");
    expect(get(w, "/machines").status).toBe(401);
    const post = (pw: string, hubHeader = "1") => w.handle("POST", "/session", { password: pw }, { hubHeader });
    expect(post(HUB_PASSWORD, "").status).toBe(400);
    const good = post(HUB_PASSWORD);
    expect(good.status).toBe(204);
    expect(get(w, "/machines", cookieOf(good.cookie)).status).toBe(200);
    expect(post("x").status).toBe(401);
    post("x");
    post("x");
    expect(post(HUB_PASSWORD).status).toBe(429);
  });

  it("refuses codes with master_key_unproven and no_package", () => {
    for (const [sc, code] of [["master_key_unproven", "master_key_unproven"], ["no_package", "no_package"]] as const) {
      const w = hubWorldFor(new URLSearchParams({ scenario: sc }));
      const r = w.handle("POST", "/join-codes", {}, { cookie: loggedIn(w), hubHeader: "1" });
      expect(r.status).toBe(409);
      expect((r.body as { code: string }).code).toBe(code);
    }
  });

  it("a created code is used by a new computer, which gets its first backup later", () => {
    const t0 = Date.now();
    const w = new HubWorld("empty", t0);
    const c = loggedIn(w);
    const created = w.handle("POST", "/join-codes", { note: "laptop" }, { cookie: c, hubHeader: "1" }, t0);
    expect(created.status).toBe(201);
    const code = created.body as { command: string; code: string; expires_at: string };
    expect(code.command).toContain(code.code);
    expect(code.command).toContain("https://hub.example.com:8443/join.sh");
    const list = (at: number) => w.handle("GET", "/machines", null, { cookie: c }, at).body as HubMachines;
    expect(list(t0 + 5_000).machines).toHaveLength(0);
    const joined = list(t0 + 21_000).machines;
    expect(joined).toHaveLength(1);
    expect(joined[0]!.last_snapshot).toBeNull();
    expect((w.handle("GET", "/join-codes", null, { cookie: c }, t0 + 21_000).body as { codes: unknown[] }).codes).toHaveLength(0);
    expect(list(t0 + 70_000).machines[0]!.snapshot_count).toBe(1);
  });

  it("removing fails with 503 while the PBS does not answer", () => {
    const w = hubWorldFor(new URLSearchParams({ scenario: "ok", fault: "remove" }));
    const r = w.handle("DELETE", "/machines/m-desktop", null, { cookie: loggedIn(w), hubHeader: "1" });
    expect(r.status).toBe(503);
    expect((r.body as { code: string }).code).toBe("pbs_unreachable");
  });

  it("uses placeholders only", () => {
    const w = new HubWorld("ok");
    const text = JSON.stringify([w.info, get(w, "/machines", loggedIn(w)).body]);
    expect(text).not.toMatch(/\b(?:\d{1,3}\.){3}\d{1,3}\b/);
    expect(text).toContain("hub.example.com");
    expect(text).toContain("pbs.example.com");
  });
});

it("explains internal hub failures in both languages without blaming PBS", () => {
  const failure = new HubFailure(500, "hub_error", "server details");
  expect(hubFailureText(failure, de)).toBe(de.hub.internal);
  expect(hubFailureText(failure, en)).toBe(en.hub.internal);
  expect(hubFailureText(new HubFailure(503, "pbs_refused", "refused"), en)).toBe("refused");
});
