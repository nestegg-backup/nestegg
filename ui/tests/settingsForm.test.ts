// Settings page helpers: choices, number input and readable problem texts.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { ApiFailure } from "../src/lib/api";
import { DEFAULT_LIMITS, overdueCap, settingsProblem } from "../src/lib/settings";
import { INTERVAL_CHOICES, intervalOptions, intervalReachable, overdueFor, parseHours, problemText, saveFailure, serialListEdits } from "../src/lib/settingsForm";
import type { SettingsLimits } from "../src/lib/types";

const current = { interval_hours: 24, overdue_after_hours: 36, wake_on_lan_configured: false };
/** `limits` of a daemon whose file has a longer "overdue after" than the API allows. */
const fromFile: SettingsLimits = {
  ...DEFAULT_LIMITS,
  overdue_after_hours: { ...DEFAULT_LIMITS.overdue_after_hours, max: 744, file_minimum: 744 },
};
/** A daemon from before the rhythm-dependent cap: only `max`. */
const onlyMax: SettingsLimits = { ...DEFAULT_LIMITS, overdue_after_hours: { min: 2, max: 168, greater_than_interval: true } };

describe("rhythm choices", () => {
  it("knows 6 h, 12 h, daily, every 2 days and weekly", () => {
    expect(INTERVAL_CHOICES).toEqual([6, 12, 24, 48, 168]);
  });

  it("offers the weekly rhythm: the cap follows the rhythm", () => {
    expect(intervalOptions(24)).toEqual([6, 12, 24, 48, 168]);
    expect(intervalReachable(168)).toBe(true);
    expect(intervalOptions(24, fromFile)).toEqual([6, 12, 24, 48, 168]);
    expect(intervalReachable(168, fromFile)).toBe(true);
  });

  it("offers only what a daemon without the numbers can follow", () => {
    expect(intervalOptions(24, onlyMax)).toEqual([6, 12, 24, 48]);
    expect(intervalReachable(167, onlyMax)).toBe(true);
    expect(intervalReachable(168, onlyMax)).toBe(false);
  });

  it("adds the current value when it was set to something else in the terminal", () => {
    expect(intervalOptions(36)).toEqual([6, 12, 24, 36, 48, 168]);
    expect(intervalOptions(null)).toEqual([6, 12, 24, 48, 168]);
    // A rhythm that is already set stays in the list, even if it could not be chosen today.
    expect(intervalOptions(168, onlyMax)).toEqual([6, 12, 24, 48, 168]);
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
    expect(overdueFor(168, 36, fromFile)).toBe(252);
    // The raised value is accepted by the same rules as the daemon's, for every rhythm on offer.
    for (const h of intervalOptions(24)) {
      const raised = overdueFor(h, 6);
      expect(settingsProblem({ interval_hours: h, overdue_after_hours: raised ?? 6 }, current)).toBeNull();
    }
    for (const h of intervalOptions(24, fromFile)) {
      const raised = overdueFor(h, 6, fromFile);
      expect(settingsProblem({ interval_hours: h, overdue_after_hours: raised ?? 6 }, current, fromFile)).toBeNull();
    }
  });

  it("never goes above the cap of the new rhythm", () => {
    // Half a rhythm on top would be 252 for weekly; the cap is 240.
    expect(overdueFor(168, 36)).toBe(240);
    expect(overdueFor(144, 36)).toBe(216);
    expect(overdueFor(168, 36, onlyMax)).toBe(168);
    // Whatever is raised is accepted by the daemon's rule for the same request, for every rhythm.
    for (const h of INTERVAL_CHOICES) {
      const raised = overdueFor(h, 6);
      expect(raised, String(h)).toBeLessThanOrEqual(overdueCap(h));
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
    { excludes: ["a\u200bb"] },
    { overdue_after_hours: 745 },
    { excludes: ["a\nb"] },
    { excludes: [" a"] },
    { excludes: ["-a"] },
    { excludes: ["**"] },
    { excludes: ["a", "a"] },
    { excludes: Array.from({ length: 201 }, (_, i) => `p${i}`) },
    { wake_on_lan: true },
    { datastore: "x" },
    { overdue_after_hours: 169 },
  ];
  /** What the daemon says about a pattern it refuses (its own wording; the browser cannot produce it). */
  const daemonRefusals = [
    "exclude pattern `/alice` would leave out the whole folder /home/alice from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.",
    "exclude pattern `/a`b` would leave out the whole folder /usr/local from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.",
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
    for (const message of daemonRefusals) expect(problemText(message, t), message).not.toBeNull();
  });

  it("explains the limit on 'overdue after' and says an administrator can lift it", () => {
    const capped = settingsProblem({ overdue_after_hours: 169 }, current)!;
    expect(problemText(capped, en)).toBe(
      "Allowed are 2 to 168 hours (7 days) here. A longer time can only be set by an administrator in /etc/nestegg/config.toml.",
    );
    expect(problemText(capped, de)).toContain("2 bis 168 Stunden (7 Tage)");
    expect(problemText(capped, de)).toContain("/etc/nestegg/config.toml");
    // The cap follows the rhythm of the same request.
    const weekly = settingsProblem({ interval_hours: 168, overdue_after_hours: 241 }, current)!;
    expect(problemText(weekly, en)).toContain("Allowed are 2 to 240 hours (10 days) here.");
    // A value from the file above the cap: the page says it can only be shortened.
    const lowered = settingsProblem({ overdue_after_hours: 745 }, { ...current, overdue_after_hours: 744 }, fromFile)!;
    expect(problemText(lowered, en)).toContain("The current value of 744 hours");
    expect(problemText(lowered, en)).toContain("only be shortened");
    expect(problemText(lowered, de)).toContain("nur verkürzen");
    // A daemon from before the limit still gets its range sentence.
    expect(problemText("overdue_after_hours must be between 2 and 744 (31 days)", en)).toBe("Allowed are 2 to 744 hours.");
  });

  it("explains the daemon's refusal of an exclude that would leave out a whole folder, in both languages", () => {
    const refusal = (pattern: string, folder: string) =>
      `exclude pattern \`${pattern}\` would leave out the whole folder ${folder} from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.`;
    expect(problemText(refusal("/alice", "/home/alice"), en)).toBe(
      "That would leave the whole folder “/home/alice” out of the backup. nestegg does not allow that here. An administrator can still set it in /etc/nestegg/config.toml.",
    );
    expect(problemText(refusal("**/alice", "/home/alice"), de)).toBe(
      "Damit würde der ganze Ordner „/home/alice“ aus der Sicherung fallen. Das lässt nestegg hier nicht zu. Ein Administrator kann es in /etc/nestegg/config.toml trotzdem festlegen.",
    );
    // A backtick in the pattern or a space in the folder does not confuse it.
    expect(problemText(refusal("/a`b", "/home/a b"), en)).toContain("“/home/a b”");
    // It arrives as a refusal from the daemon and is shown as a sentence, not as the raw message.
    const failure = saveFailure(new ApiFailure("bad_request", 400, null, refusal("/alice", "/home/alice")), en);
    expect(failure.detail).toBeNull();
    expect(failure.text).toContain("“/home/alice”");
  });

  it("names the byte limit and the daemon's wording of an older limit", () => {
    expect(problemText("exclude patterns may have at most 256 bytes", en)).toContain("at most 256 bytes");
    expect(problemText("exclude patterns may have at most 256 characters", de)).toContain("256 Byte");
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
    // The daemon's messages as they are since the whole list is judged together.
    expect(
      saveFailure(
        f(
          "bad_request",
          400,
          "this exclude list would leave out the whole folder /home/alice from the backup; an administrator can still set it as root in /etc/nestegg/config.toml.",
        ),
        de,
      ),
    ).toEqual({ text: p.excludeFolder("/home/alice"), detail: null });
    for (const message of [
      "exclude pattern `[abc` is malformed (an unclosed `[`…`]` character class or a dangling `\\`, for example) and would break every backup run",
      "exclude pattern `foo\\` is not a valid pattern (trailing backslash); the backup client would reject it",
    ]) {
      expect(saveFailure(f("bad_request", 400, message), de)).toEqual({ text: p.excludeMalformed, detail: null });
    }
  });

  it("shows the daemon's own message when the reason is unknown", () => {
    expect(saveFailure(f("bad_request", 400, "brand new rule"), de)).toEqual({ text: de.settings.problem.other, detail: "brand new rule" });
    expect(saveFailure(f("other", 500, "disk full"), de)).toEqual({ text: de.settings.problem.other, detail: "disk full" });
  });
});

describe("edits of the exclude list run one after the other", () => {
  /** A daemon that answers after a moment, like the real one. */
  function daemon(initial: string[]) {
    const state = { list: [...initial], saves: [] as string[][] };
    const later = <T>(v: T) => new Promise<T>((r) => setTimeout(() => r(v), 5));
    return {
      state,
      load: () => later([...state.list]),
      save: async (list: string[]) => {
        await later(null);
        state.list = [...list];
        state.saves.push([...list]);
        return true;
      },
    };
  }

  it("two removals started together remove both (the second starts from the first one's result)", async () => {
    const d = daemon(["A", "B"]);
    const busy: boolean[] = [];
    const edit = serialListEdits(d.load, (on) => busy.push(on));
    const remove = (p: string) => edit((list) => d.save(list.filter((x) => x !== p)));
    // Not awaited one by one: both are under way before the first has loaded anything.
    const both = Promise.all([remove("A"), remove("B")]);
    expect(busy).toEqual([true, true]);
    expect(await both).toEqual([true, true]);
    expect(d.state.saves).toEqual([["B"], []]);
    expect(d.state.list).toEqual([]);
    // Busy the whole time, also between the two saves, and free at the end.
    expect(busy).toEqual([true, true, true, false]);
  });

  it("an addition and a removal started together both take effect", async () => {
    const d = daemon(["A"]);
    const edit = serialListEdits(d.load, () => {});
    await Promise.all([
      edit((list) => d.save([...list, "C"])),
      edit((list) => d.save(list.filter((x) => x !== "A"))),
    ]);
    expect(d.state.list).toEqual(["C"]);
  });

  it("an edit that fails does not hold up the next one", async () => {
    const d = daemon(["A", "B"]);
    const busy: boolean[] = [];
    const edit = serialListEdits(d.load, (on) => busy.push(on));
    const failing = edit(async () => {
      throw new Error("no answer");
    });
    const next = edit((list) => d.save(list.filter((x) => x !== "B")));
    await expect(failing).rejects.toThrow("no answer");
    expect(await next).toBe(true);
    expect(d.state.list).toEqual(["A"]);
    expect(busy.at(-1)).toBe(false);
  });
});
