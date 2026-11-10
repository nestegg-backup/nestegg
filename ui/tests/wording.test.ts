// Texts and rows that the review found wrong: what the lead says above a notice, and whether the
// history, the strip and the restore picker tell the same story.
import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { allDamaged, runEvents, serverBackups, succeededKind } from "../src/lib/derive";
import { createFormatter } from "../src/lib/format";
import { selfSummary } from "../src/lib/text";
import type { Run, Status } from "../src/lib/types";

const f = createFormatter("de", "de-DE", "Europe/Berlin");
const now = new Date("2026-11-08T18:00:00Z");

function run(over: Partial<Run> = {}): Run {
  return { id: "r", trigger: "timer", state: "succeeded", started_at: "2026-11-08T00:42:00Z", snapshot: "host/desktop/2026-11-08T00:42:00Z", ...over };
}

function status(over: Partial<Status> = {}): Status {
  return {
    machine: { hostname: "desktop", backup_id: "desktop" },
    pbs: { server: "pbs.example.com:8007", datastore: "backups" },
    schedule: { enabled: true },
    encryption: { enabled: true, key_fingerprint: "<FINGERPRINT>", key_exported: true },
    health: "ok",
    last_success: run(),
    ...over,
  };
}

describe("the lead above a damaged-backups notice", () => {
  it("does not say that everything is fine", () => {
    const damaged = status({ server_verify_failed: ["host/desktop/2026-11-05T00:42:00Z"] });
    for (const t of [de, en]) {
      const s = selfSummary(damaged, t, f, now);
      expect(s.text).not.toContain(t.self.okText);
      expect(s.text).toContain(t.self.okTextDamaged);
    }
    expect(selfSummary(status(), de, f, now).text).toContain(de.self.okText);
  });

  it("also when only the snapshot list knows about the damage", () => {
    const list = serverBackups({ state: "ok", list: [{ time: "2026-11-08T00:42:00Z", verified: "failed" }] });
    expect(list?.damaged).toBe(1);
    expect(selfSummary(status(), en, f, now, list).text).not.toContain(en.self.okText);
  });
});

describe("health incomplete", () => {
  it("points at the list below only when there is one", () => {
    const withList = status({ health: "incomplete", last_success: run({ incomplete: ["optional source /mnt/data does not exist, skipped"] }) });
    const noList = status({ health: "incomplete", last_success: run({ incomplete: [] }) });
    const noRun = status({ health: "incomplete", last_success: undefined });
    for (const t of [de, en]) {
      expect(selfSummary(withList, t, f, now).text).toContain(t.self.incompleteText);
      for (const s of [noList, noRun]) {
        const text = selfSummary(s, t, f, now).text;
        expect(text).toContain(t.self.incompleteTextNoList);
        expect(text).not.toContain(t.self.incompleteText);
      }
    }
    expect(en.self.incompleteTextNoList).not.toMatch(/below/);
    expect(de.self.incompleteTextNoList).not.toMatch(/unten/);
  });
});

describe("history row and strip agree", () => {
  const damaged = new Set(["host/desktop/2026-11-08T00:42:00Z"]);

  it("a damaged snapshot is damaged in both, even when the run has a note or is incomplete", () => {
    for (const r of [run(), run({ warnings: ["a note"] }), run({ incomplete: ["x"], warnings: ["x"] })]) {
      expect(succeededKind(r, damaged)).toBe("damaged");
      expect(runEvents([r], null, damaged)[0]!.kind).toBe("damaged");
    }
  });

  it("without damage: incomplete, notes and ok", () => {
    expect(succeededKind(run({ incomplete: ["x"], warnings: ["x"] }))).toBe("incomplete");
    expect(succeededKind(run({ warnings: ["a note"] }))).toBe("warnings");
    expect(succeededKind(run())).toBe("ok");
    // A note is still an ok day in the strip.
    expect(runEvents([run({ warnings: ["a note"] })])[0]!.kind).toBe("ok");
  });

  it("the history has a text for it, in both languages", () => {
    expect(de.runs.damaged).toMatch(/beschädigt/);
    expect(en.runs.damaged).toMatch(/damaged/);
  });
});

describe("restore picker", () => {
  it("knows when every snapshot is damaged, so it cannot advise 'take another one'", () => {
    const a = { snapshot: "host/desktop/a", verified: "failed" as const };
    const b = { snapshot: "host/desktop/b", verified: "ok" as const };
    expect(allDamaged([a, b])).toBe(false);
    expect(allDamaged([a, { snapshot: "host/desktop/b" }], ["host/desktop/b"])).toBe(true);
    expect(allDamaged([])).toBe(false);
    for (const t of [de, en]) {
      expect(t.restore.damagedAll).not.toBe(t.restore.damagedSnapshot);
    }
    expect(en.restore.damagedAll).not.toMatch(/another/i);
    expect(de.restore.damagedAll).not.toMatch(/anderen/i);
  });
});

describe("words", () => {
  it("copy buttons name what they copy", () => {
    expect(en.app.copyOf("sudo nestegg check")).toBe("Copy command: sudo nestegg check");
    expect(de.app.copyOf("sudo nestegg check")).toBe("Befehl kopieren: sudo nestegg check");
    expect(en.app.copyOf("x").startsWith(en.app.copy)).toBe(true);
    expect(de.app.copyOf("x").startsWith(de.app.copy)).toBe(true);
  });

  it("German uses 'Secret' for the token's secret, never 'Geheimnis'", () => {
    expect(JSON.stringify(de)).not.toMatch(/Geheimnis/);
  });

  it("the upload progress reads naturally", () => {
    expect(de.self.stepUploadProgress("12 GB", "412 GB")).toBe("12 GB von etwa 412 GB verglichen");
    expect(en.self.stepUploadProgress("12 GB", "412 GB")).toBe("12 GB of about 412 GB compared");
    expect(de.self.stepUploadProgress("a", "b")).not.toContain("durchgesehen");
  });

  it("cancel texts: 409 cannot_cancel (starting or over) and the broad 404 text of older daemons", () => {
    expect(en.self.cancelCannot).toMatch(/only just starting/);
    expect(de.self.cancelCannot).toMatch(/startet erst/);
    expect(en.self.cancelGone).toMatch(/no longer running/);
  });
});
