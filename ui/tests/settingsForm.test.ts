// Settings page helpers: choices, number input and readable problem texts.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { ApiFailure } from "../src/lib/api";
import { settingsProblem } from "../src/lib/settings";
import { INTERVAL_CHOICES, intervalOptions, overdueFor, parseHours, problemText, saveFailure } from "../src/lib/settingsForm";

const current = { interval_hours: 24, overdue_after_hours: 36, wake_on_lan_configured: false };

describe("rhythm choices", () => {
  it("offers 6 h, 12 h, daily, every 2 days and weekly", () => {
    expect(INTERVAL_CHOICES).toEqual([6, 12, 24, 48, 168]);
    expect(intervalOptions(24)).toEqual([6, 12, 24, 48, 168]);
  });

  it("adds the current value when it was set to something else in the terminal", () => {
    expect(intervalOptions(36)).toEqual([6, 12, 24, 36, 48, 168]);
    expect(intervalOptions(null)).toEqual([6, 12, 24, 48, 168]);
  });

  it("names the choices", () => {
    expect([6, 12, 24, 48, 168, 36, 1].map((h) => de.settings.everyHours(h))).toEqual([
      "Alle 6 Stunden",
      "Alle 12 Stunden",
      "Täglich",
      "Alle 2 Tage",
      "Wöchentlich",
      "Alle 36 Stunden",
      "Stündlich",
    ]);
    expect(en.settings.everyHours(48)).toBe("Every 2 days");
  });
});

describe("hours field", () => {
  it("takes whole numbers only", () => {
    expect(parseHours(" 36 ")).toBe(36);
    expect(parseHours("0")).toBe(0);
    for (const v of ["", "3.5", "-1", "1e3", "abc", "12h"]) expect(parseHours(v), v).toBeNull();
  });
});

describe("overdue threshold with a longer rhythm", () => {
  it("stays when it is still longer than the rhythm", () => {
    expect(overdueFor(12, 36)).toBeNull();
    expect(overdueFor(24, 36)).toBeNull();
  });

  it("is raised by half a rhythm, at least 12 hours, within the limit", () => {
    expect(overdueFor(48, 36)).toBe(72);
    expect(overdueFor(36, 36)).toBe(54);
    expect(overdueFor(24, 20)).toBe(36);
    expect(overdueFor(168, 36)).toBe(252);
    // The raised value is accepted by the same rules as the daemon's.
    for (const h of INTERVAL_CHOICES) {
      const raised = overdueFor(h, 6);
      expect(settingsProblem({ interval_hours: h, overdue_after_hours: raised ?? 6 }, current)).toBeNull();
    }
  });
});

describe("problem texts", () => {
  // Every message settings.ts (and with it the daemon) can produce for a field of the page.
  const bodies: unknown[] = [
    { interval_hours: 0 },
    { overdue_after_hours: 1 },
    { overdue_after_hours: 12 },
    { interval_hours: 1.5 },
    { excludes: [""] },
    { excludes: ["a".repeat(300)] },
    { excludes: ["a\nb"] },
    { excludes: [" a"] },
    { excludes: ["-a"] },
    { excludes: ["**"] },
    { excludes: ["a", "a"] },
    { excludes: Array.from({ length: 201 }, (_, i) => `p${i}`) },
    { wake_on_lan: true },
    { datastore: "x" },
  ];

  it.each([
    ["de", de],
    ["en", en],
  ])("has a %s text for every validation message", (_lang, t) => {
    for (const body of bodies) {
      const message = settingsProblem(body, current);
      expect(message, JSON.stringify(body)).not.toBeNull();
      expect(problemText(message!, t), message!).not.toBeNull();
    }
  });

  it("keeps the numbers of the message", () => {
    expect(problemText(settingsProblem({ overdue_after_hours: 12 }, current)!, de)).toContain("(24 Stunden)");
    expect(problemText("interval_hours must be between 1 and 168 (one week)", de)).toBe(
      "Der Rhythmus muss zwischen 1 und 168 Stunden liegen.",
    );
    expect(problemText("something new", de)).toBeNull();
  });
});

describe("failed saves", () => {
  const f = (kind: ApiFailure["kind"], status: number, message: string) => new ApiFailure(kind, status, null, message);

  it("explains each kind of failure", () => {
    const p = de.settings.problem;
    expect(saveFailure(f("daemon", 502, "cannot connect"), de)).toEqual({ text: p.daemon, detail: null });
    expect(saveFailure(f("unauthorized", 401, "x"), de)).toEqual({ text: p.session, detail: null });
    expect(saveFailure(f("other", 403, "caller unknown"), de)).toEqual({ text: p.forbidden, detail: "caller unknown" });
    expect(
      saveFailure(f("other", 500, "cannot switch automatic backups off; nothing was changed: systemctl failed"), de).text,
    ).toBe(p.timer);
    expect(
      saveFailure(
        f("other", 500, "automatic backups were switched on and the settings saved, but the timer could not be started: x"),
        de,
      ).text,
    ).toBe(p.timerHalf);
    expect(saveFailure(f("other", 500, "invalid configuration /etc/nestegg/config.toml"), de).text).toBe(p.configBroken);
    expect(saveFailure(f("bad_request", 400, "exclude pattern `**` would exclude everything; the backups would be empty"), de)).toEqual({
      text: p.excludeEverything,
      detail: null,
    });
  });

  it("shows the daemon's own message when the reason is unknown", () => {
    expect(saveFailure(f("bad_request", 400, "brand new rule"), de)).toEqual({ text: de.settings.problem.other, detail: "brand new rule" });
    expect(saveFailure(f("other", 500, "disk full"), de)).toEqual({ text: de.settings.problem.other, detail: "disk full" });
  });
});
