// Date, size and duration formatting. Everything takes the language, locale
// and (optionally) a time zone so it can be tested deterministically.

import { messages, type Lang } from "../i18n";

export interface Moment {
  /** Relation to "today" in the formatter's time zone. */
  rel: "today" | "yesterday" | "tomorrow" | "other";
  /** "Sonntag, 8. November" / "Sunday 8 November" */
  day: string;
  /** "01:42" */
  time: string;
}

export interface Formatter {
  lang: Lang;
  locale: string;
  /** "01:42" */
  time(d: Date | string): string;
  /** "So 8. Nov." / "Sun 8 Nov" */
  dayShort(d: Date | string): string;
  /** "Sonntag, 8. November" / "Sunday 8 November" */
  dayLong(d: Date | string): string;
  /** "Sonntag, 8. November 2026" / "Sunday 8 November 2026" */
  dayLongYear(d: Date | string): string;
  /** "8. Nov." / "8 Nov" */
  dayMonth(d: Date | string): string;
  /** "8" */
  dayNumber(d: Date | string): string;
  /** "Oktober 2026" / "October 2026" */
  monthYear(d: Date | string): string;
  /** Local calendar day as YYYY-MM-DD. */
  dayKey(d: Date | string): string;
  /** Labels for a calendar day given as YYYY-MM-DD, without any time zone conversion. */
  keyShort(key: string): string;
  keyLong(key: string): string;
  keyNumber(key: string): string;
  moment(d: Date | string, now: Date): Moment;
  size(bytes: number): string;
  duration(seconds: number): string;
  number(n: number): string;
}

const toDate = (d: Date | string) => (typeof d === "string" ? new Date(d) : d);

const DAY_MS = 86_400_000;

/** Whole calendar days between two YYYY-MM-DD keys (b - a). */
export function daysBetween(a: string, b: string): number {
  const pa = Date.UTC(+a.slice(0, 4), +a.slice(5, 7) - 1, +a.slice(8, 10));
  const pb = Date.UTC(+b.slice(0, 4), +b.slice(5, 7) - 1, +b.slice(8, 10));
  return Math.round((pb - pa) / DAY_MS);
}

/** Shift a YYYY-MM-DD key by n days. */
export function shiftDayKey(key: string, n: number): string {
  const t = Date.UTC(+key.slice(0, 4), +key.slice(5, 7) - 1, +key.slice(8, 10)) + n * DAY_MS;
  return new Date(t).toISOString().slice(0, 10);
}

export function createFormatter(lang: Lang, locale: string, timeZone?: string): Formatter {
  const tz = timeZone ? { timeZone } : {};
  const timeF = new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit", hourCycle: lang === "de" ? "h23" : undefined, ...tz });
  const partsF = new Intl.DateTimeFormat(lang === "de" ? "de-DE" : "en-GB", { weekday: "short", day: "numeric", month: "short", ...tz });
  const longF = new Intl.DateTimeFormat(locale, { weekday: "long", day: "numeric", month: "long", ...tz });
  const longYF = new Intl.DateTimeFormat(locale, { weekday: "long", day: "numeric", month: "long", year: "numeric", ...tz });
  const monthYearF = new Intl.DateTimeFormat(locale, { month: "long", year: "numeric", ...tz });
  const keyF = new Intl.DateTimeFormat("en-CA", { year: "numeric", month: "2-digit", day: "2-digit", ...tz });
  const num1 = new Intl.NumberFormat(locale, { maximumFractionDigits: 1, minimumFractionDigits: 1 });
  const num0 = new Intl.NumberFormat(locale, { maximumFractionDigits: 0 });

  const parts = (d: Date) => {
    const p = partsF.formatToParts(d);
    const get = (t: string) => (p.find((x) => x.type === t)?.value ?? "").replace(/\.$/, "");
    return { wd: get("weekday"), day: get("day"), mon: p.find((x) => x.type === "month")?.value ?? "" };
  };
  const dayKey = (d: Date | string) => {
    const p = keyF.formatToParts(toDate(d));
    const g = (t: string) => p.find((x) => x.type === t)?.value ?? "";
    return `${g("year")}-${g("month")}-${g("day")}`;
  };

  // A YYYY-MM-DD key is a calendar date, not an instant: format it as noon UTC in UTC,
  // so it never moves to another day whatever the time zone.
  const keyDate = (key: string) => new Date(`${key}T12:00:00Z`);
  const utc = timeZone === "UTC" ? null : createFormatter(lang, locale, "UTC");

  const f: Formatter = {
    lang,
    locale,
    time: (d) => timeF.format(toDate(d)),
    dayShort: (d) => {
      const { wd, day, mon } = parts(toDate(d));
      // German month abbreviations keep their dot ("Nov."), except short ones ("Mai").
      return lang === "de" ? `${wd} ${day}. ${mon}` : `${wd} ${day} ${mon.replace(/\.$/, "")}`;
    },
    dayLong: (d) => longF.format(toDate(d)),
    dayLongYear: (d) => longYF.format(toDate(d)),
    dayMonth: (d) => {
      const { day, mon } = parts(toDate(d));
      return lang === "de" ? `${day}. ${mon}` : `${day} ${mon.replace(/\.$/, "")}`;
    },
    dayNumber: (d) => parts(toDate(d)).day,
    monthYear: (d) => monthYearF.format(toDate(d)),
    dayKey,
    keyShort: (key) => (utc ?? f).dayShort(keyDate(key)),
    keyLong: (key) => (utc ?? f).dayLong(keyDate(key)),
    keyNumber: (key) => String(Number(key.slice(8, 10))),
    moment: (d, now) => {
      const date = toDate(d);
      const diff = daysBetween(dayKey(now), dayKey(date));
      const rel = diff === 0 ? "today" : diff === -1 ? "yesterday" : diff === 1 ? "tomorrow" : "other";
      return { rel, day: longF.format(date), time: timeF.format(date) };
    },
    size: (bytes) => {
      // Decimal units, as file managers on Linux show them.
      const units = ["B", "KB", "MB", "GB", "TB", "PB"];
      let v = Math.max(0, bytes);
      let i = 0;
      while (v >= 1000 && i < units.length - 1) {
        v /= 1000;
        i++;
      }
      if (i === 0) return `${num0.format(v)} B`;
      // Round first so 999.95 MB becomes "1,0 GB" and not "1000 MB".
      if (v >= 10 && Math.round(v) >= 1000 && i < units.length - 1) {
        v /= 1000;
        i++;
      }
      return `${v >= 10 ? num0.format(v) : num1.format(v)} ${units[i]}`;
    },
    duration: (seconds) => {
      const d = messages[lang].duration;
      const s = Math.max(0, Math.round(seconds));
      if (s < 60) return d.seconds(s);
      const m = Math.round(s / 60);
      if (m < 60) return d.minutes(m);
      const h = Math.floor(m / 60);
      const rm = m % 60;
      return rm ? d.hoursMinutes(h, rm) : d.hours(h);
    },
    number: (n) => num0.format(n),
  };
  return f;
}
