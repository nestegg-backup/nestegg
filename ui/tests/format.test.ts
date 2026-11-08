import { describe, expect, it } from "vitest";
import { createFormatter, daysBetween, shiftDayKey } from "../src/lib/format";

const de = createFormatter("de", "de-DE", "Europe/Berlin");
const en = createFormatter("en", "en-GB", "Europe/Berlin");
const us = createFormatter("en", "en-US", "Europe/Berlin");

describe("dates", () => {
  const t = "2026-11-08T00:42:00Z"; // Sunday 01:42 in Berlin

  it("formats like the prototype in German", () => {
    expect(de.dayShort(t)).toBe("So 8. Nov.");
    expect(de.dayLong(t)).toBe("Sonntag, 8. November");
    expect(de.dayLongYear(t)).toBe("Sonntag, 8. November 2026");
    expect(de.dayMonth(t)).toBe("8. Nov.");
    expect(de.time(t)).toBe("01:42");
    expect(de.dayMonth("2026-05-03T10:00:00Z")).toBe("3. Mai");
  });

  it("formats in English", () => {
    expect(en.dayShort(t)).toBe("Sun 8 Nov");
    expect(en.dayMonth(t)).toBe("8 Nov");
    expect(en.time(t)).toBe("01:42");
    expect(us.time(t)).toMatch(/^01:42\s?AM$/);
  });

  it("uses the local calendar day, not the UTC one", () => {
    expect(de.dayKey("2026-11-07T23:30:00Z")).toBe("2026-11-08");
    expect(createFormatter("de", "de-DE", "UTC").dayKey("2026-11-07T23:30:00Z")).toBe("2026-11-07");
  });

  it("knows today, yesterday and tomorrow", () => {
    const now = new Date("2026-11-08T18:00:00Z");
    expect(de.moment("2026-11-08T00:42:00Z", now).rel).toBe("today");
    expect(de.moment("2026-11-07T23:30:00Z", now).rel).toBe("today"); // 00:30 local
    expect(de.moment("2026-11-07T12:00:00Z", now).rel).toBe("yesterday");
    expect(de.moment("2026-11-09T01:00:00Z", now).rel).toBe("tomorrow");
    expect(de.moment("2026-11-01T01:00:00Z", now)).toEqual({ rel: "other", day: "Sonntag, 1. November", time: "02:00" });
  });

  it("does calendar arithmetic across DST changes", () => {
    expect(daysBetween("2026-10-24", "2026-10-26")).toBe(2);
    expect(shiftDayKey("2026-03-01", -1)).toBe("2026-02-28");
    expect(shiftDayKey("2026-10-25", 1)).toBe("2026-10-26");
  });
});

describe("sizes", () => {
  it("uses decimal units and the locale's separator", () => {
    expect(de.size(0)).toBe("0 B");
    expect(de.size(999)).toBe("999 B");
    expect(de.size(1_234)).toBe("1,2 KB");
    expect(de.size(1_200_000_000)).toBe("1,2 GB");
    expect(de.size(412_000_000_000)).toBe("412 GB");
    expect(de.size(1_100_000_000_000)).toBe("1,1 TB");
    expect(en.size(1_200_000_000)).toBe("1.2 GB");
  });

  it("does not show 1000 of a unit", () => {
    expect(de.size(999_600_000)).toBe("1,0 GB");
  });
});

describe("durations", () => {
  it("is short and readable", () => {
    expect(de.duration(40)).toBe("40 Sek.");
    expect(de.duration(14 * 60)).toBe("14 Min.");
    expect(de.duration(65 * 60)).toBe("1 Std. 5 Min.");
    expect(de.duration(120 * 60)).toBe("2 Std.");
    expect(en.duration(40)).toBe("40 s");
    expect(en.duration(14 * 60)).toBe("14 min");
    expect(en.duration(65 * 60)).toBe("1 h 5 min");
  });
});
