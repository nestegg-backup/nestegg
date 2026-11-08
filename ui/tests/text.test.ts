import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { createFormatter } from "../src/lib/format";
import { machineSummary, selfRowText, selfSummary, serverRow } from "../src/lib/text";
import type { Run, Status } from "../src/lib/types";

const fde = createFormatter("de", "de-DE", "Europe/Berlin");
const fen = createFormatter("en", "en-GB", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");

const ok: Run = { id: "a", trigger: "timer", state: "succeeded", started_at: "2026-11-08T00:42:00Z", finished_at: "2026-11-08T00:54:00Z" };
const failed: Run = {
  id: "b",
  trigger: "timer",
  state: "failed",
  started_at: "2026-11-08T00:31:00Z",
  error: { kind: "source_missing", message: "source /mnt/fotos missing" },
};
const unreachable: Run = { id: "c", trigger: "timer", state: "pbs_unreachable", started_at: "2026-11-08T10:30:00Z" };

function status(over: Partial<Status>): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups", wake_on_lan: true },
    schedule: { enabled: true, next_run: "2026-11-09T00:30:00Z" },
    encryption: { enabled: true, key_exported: true },
    health: "ok",
    last_run: ok,
    last_success: ok,
    ...over,
  };
}

describe("status sentence of this PC", () => {
  it("ok: matches the prototype wording", () => {
    const s = selfSummary(status({}), de, fde, now);
    expect(s.lead).toBe("Heute um 01:42 gesichert.");
    expect(s.text).toBe(
      "Alles in Ordnung. Die nächste Sicherung ist morgen um 01:30 fällig. Ist der PC dann aus, wird sie beim nächsten Einschalten nachgeholt.",
    );
    expect(selfRowText(status({}), de, fde, now)).toBe("Heute 01:42 gesichert");
    expect(selfSummary(status({}), en, fen, now).lead).toBe("Backed up today at 01:42.");
  });

  it("ok, but the last attempt did not reach the server", () => {
    const s = selfSummary(status({ last_run: unreachable }), de, fde, now);
    expect(s.text).toContain("Der letzte Versuch heute um 11:30 hat den Backup-Server nicht erreicht.");
  });

  it("failing: names the day and explains the reason", () => {
    const yesterday: Run = { ...ok, started_at: "2026-11-07T00:45:00Z" };
    const s = selfSummary(status({ health: "failing", last_run: failed, last_success: yesterday }), de, fde, now);
    expect(s.lead).toBe("Die Sicherung heute um 01:31 ist fehlgeschlagen.");
    expect(s.text).toContain("Ein Ordner, der gesichert werden soll, fehlte");
    expect(s.text).toContain("Zuletzt gesichert gestern um 01:45.");
    expect(selfRowText(status({ health: "failing", last_run: failed }), de, fde, now)).toBe("Fehlgeschlagen am 8. Nov.");
  });

  it("overdue: counts days and blames the server when it was unreachable", () => {
    const old: Run = { ...ok, started_at: "2026-11-05T00:40:00Z", finished_at: "2026-11-05T00:50:00Z" };
    const s = selfSummary(status({ health: "overdue", last_success: old, last_run: unreachable }), de, fde, now);
    expect(s.lead).toBe("Seit 3 Tagen nicht gesichert.");
    expect(s.text).toContain("Backup-Server nicht erreichbar");
    expect(selfSummary(status({ health: "overdue", last_success: old, last_run: old }), en, fen, now).lead).toBe("Not backed up for 3 days.");
  });

  it("never, unconfigured, running and missing key copy", () => {
    expect(selfSummary(status({ health: "never", last_run: null, last_success: null }), de, fde, now).lead).toBe("Noch keine Sicherung.");
    expect(selfSummary(status({ health: "unconfigured" }), de, fde, now).lead).toBe("nestegg ist auf diesem PC noch nicht eingerichtet.");
    expect(selfSummary(status({ health: "running" }), de, fde, now).lead).toBe("Sicherung läuft.");
    const k = selfSummary(status({ health: "never", last_success: null, encryption: { enabled: true, key_exported: false } }), de, fde, now);
    expect(k.kind).toBe("key_missing");
    expect(k.lead).toBe("Sicherungen warten auf die Bestätigung der Schlüsselkopie.");
  });

  it("mentions a switched-off schedule", () => {
    const s = selfSummary(status({ schedule: { enabled: false } }), de, fde, now);
    expect(s.text).toContain("Die automatische Sicherung ist ausgeschaltet.");
  });
});

describe("other PCs and server", () => {
  it("other PC that failed today", () => {
    const s = machineSummary(
      {
        backup_id: "media",
        last_snapshot: "2026-11-07T00:51:00Z",
        report: { reported_at: "2026-11-08T00:36:00Z", result: "failed", run_started_at: "2026-11-08T00:31:00Z", error: { kind: "source_missing", message: "x" } },
      },
      de,
      fde,
      now,
    );
    expect(s.lead).toBe("Die Sicherung heute um 01:31 ist fehlgeschlagen.");
    expect(s.text).toContain("Zuletzt erfolgreich gesichert gestern um 01:51.");
  });

  it("server line in the list states the last check, with time", () => {
    const st = status({});
    expect(serverRow({ ...st, pbs: { ...st.pbs, reachable: true, checked_at: "2026-11-08T00:42:00Z" } }, de, fde, now).text).toBe("Erreichbar, geprüft um 01:42");
    // Wake-on-LAN does not turn "not reachable" into "asleep".
    expect(serverRow({ ...st, pbs: { ...st.pbs, reachable: false, checked_at: "2026-11-08T13:30:00Z", wake_on_lan: true } }, de, fde, now)).toEqual({
      tone: "pbs",
      text: "Nicht erreichbar, geprüft um 14:30",
    });
    expect(serverRow({ ...st, pbs: { ...st.pbs, reachable: false, checked_at: "2026-11-05T13:30:00Z" } }, en, fen, now).text).toBe("Not reachable, checked on 5 Nov");
    expect(serverRow({ ...st, pbs: { ...st.pbs, reachable: null } }, de, fde, now).text).toBe("Noch nicht geprüft");
  });
});

describe("schedule switched off: no promise of automatic retries or next runs", () => {
  const promises = /versucht es|nächste Sicherung ist|nachgeholt|holt nestegg|tries again|next backup is due|catches up/;
  const off = { enabled: false, next_run: "2026-11-09T00:30:00Z" };
  const old: Run = { ...ok, started_at: "2026-11-05T00:40:00Z", finished_at: "2026-11-05T00:50:00Z" };
  const cases: [string, Partial<Status>][] = [
    ["ok", {}],
    ["ok after unreachable server", { last_run: unreachable }],
    ["overdue", { health: "overdue", last_success: old, last_run: old }],
    ["overdue after unreachable server", { health: "overdue", last_success: old, last_run: unreachable }],
    ["never", { health: "never", last_run: null, last_success: null }],
    ["never after unreachable server", { health: "never", last_run: unreachable, last_success: null }],
    ["failing", { health: "failing", last_run: failed }],
  ];
  it.each(cases)("%s", (_name, over) => {
    for (const [msgs, fmt] of [[de, fde], [en, fen]] as const) {
      const s = selfSummary(status({ ...over, schedule: off }), msgs, fmt, now);
      expect(`${s.lead} ${s.text}`).not.toMatch(promises);
    }
  });

  it("with the schedule on, an unreachable server is retried automatically", () => {
    const old2: Run = { ...ok, started_at: "2026-11-05T00:40:00Z" };
    expect(selfSummary(status({ health: "overdue", last_success: old2, last_run: unreachable }), de, fde, now).text).toContain(
      "nestegg versucht es stündlich von selbst wieder.",
    );
  });
});

describe("contract-valid minimal status objects", () => {
  // Only the required fields of the Status schema; every optional field is missing.
  const minimal = (health: Status["health"]): Status => ({
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true },
    encryption: { enabled: true },
    health,
  });
  const healths: Status["health"][] = ["ok", "running", "overdue", "failing", "never", "unconfigured"];

  it.each(healths)("health=%s renders without run details", (h) => {
    for (const [msgs, fmt] of [[de, fde], [en, fen]] as const) {
      for (const st of [minimal(h), { ...minimal(h), last_run: null, last_success: null, current_run: null }]) {
        const s = selfSummary(st, msgs, fmt, now);
        expect(s.lead.length).toBeGreaterThan(0);
        expect(s.lead + s.text).not.toMatch(/undefined|null|NaN|Invalid/);
        expect(selfRowText(st, msgs, fmt, now)).not.toMatch(/undefined|null|NaN|Invalid/);
        expect(serverRow(st, msgs, fmt, now).text.length).toBeGreaterThan(0);
      }
    }
  });

  it("health=failing without run details gets an undated failure text", () => {
    expect(selfSummary(minimal("failing"), de, fde, now).lead).toBe("Die letzte Sicherung ist fehlgeschlagen.");
    expect(selfRowText(minimal("failing"), en, fen, now)).toBe("Last backup failed");
  });
});

describe("key copy wording", () => {
  it("does not claim that the key exists only on the computer", () => {
    for (const msgs of [de, en]) {
      const all = [msgs.key.title, msgs.key.text, msgs.key.otherTitle, msgs.key.otherText("media"), msgs.self.keyLead, msgs.row.keyMissing].join(" ");
      expect(all).not.toMatch(/liegt nur|nur auf dem PC|only exists|fehlt\b|missing/i);
      expect(all).toMatch(/bestätigt|confirmed/);
    }
  });
});
