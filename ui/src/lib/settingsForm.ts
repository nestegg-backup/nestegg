// Helpers of the settings page (decision 0013/0014): choices, number input,
// and readable texts for the problems that settings.ts and the daemon report
// (both in English, with the same wording).

import type { Messages } from "../i18n";
import type { ApiFailure } from "./api";
import { DEFAULT_LIMITS } from "./settings";
import type { SettingsLimits } from "./types";

/** Rhythm choices of the select: 6 h, 12 h, daily, every 2 days, weekly. */
export const INTERVAL_CHOICES: readonly number[] = [6, 12, 24, 48, 168];

/** The choices, plus the current value when it is something else (set in the terminal). */
export function intervalOptions(current: number | null | undefined): number[] {
  const list = [...INTERVAL_CHOICES];
  if (current != null && !list.includes(current)) list.push(current);
  return list.sort((a, b) => a - b);
}

/** Whole hours typed into a field, or null. */
export function parseHours(raw: string): number | null {
  const v = raw.trim();
  return /^\d{1,6}$/.test(v) ? Number(v) : null;
}

/**
 * The overdue threshold must stay above the rhythm. When a longer rhythm is
 * picked, the page raises the threshold in the same request (half a rhythm,
 * at least 12 hours, on top) instead of refusing the choice. Null when the
 * current threshold still fits.
 */
export function overdueFor(interval: number, overdue: number, limits: SettingsLimits = DEFAULT_LIMITS): number | null {
  if (limits.overdue_after_hours.greater_than_interval === false || overdue > interval) return null;
  return Math.min(limits.overdue_after_hours.max, interval + Math.max(12, Math.ceil(interval / 2)));
}

const num = (s: string | undefined) => (s === undefined ? NaN : Number(s));

/**
 * Text for a validation problem (an English message from settings.ts or a
 * 400 of the daemon). Null when the message is not one of the known ones.
 */
export function problemText(message: string, t: Messages): string | null {
  const p = t.settings.problem;
  let m: RegExpExecArray | null;
  if ((m = /^interval_hours must be between (\d+) and (\d+)/.exec(message))) return p.intervalRange(num(m[1]), num(m[2]));
  if ((m = /^overdue_after_hours must be between (\d+) and (\d+)/.exec(message))) return p.overdueRange(num(m[1]), num(m[2]));
  if ((m = /must be greater than interval_hours \((\d+)\)/.exec(message))) return p.overdueNotGreater(num(m[1]));
  if (/must be a non-negative integer/.test(message)) return p.wholeNumber;
  if (/^exclude patterns must not be empty/.test(message)) return p.excludeEmpty;
  if ((m = /^exclude patterns may have at most (\d+) characters/.exec(message))) return p.excludeTooLong(num(m[1]));
  if (/contains a control character/.test(message)) return p.excludeControl;
  if (/must not start or end with spaces/.test(message)) return p.excludeSpaces;
  if (/must not start with `-`/.test(message)) return p.excludeDash;
  if (/would exclude everything/.test(message)) return p.excludeEverything;
  if (/is listed twice/.test(message)) return p.excludeDuplicate;
  if ((m = /^at most (\d+) exclude patterns/.exec(message))) return p.excludeTooMany(num(m[1]));
  if (/^Wake-on-LAN is not set up/.test(message)) return p.wakeNotConfigured;
  if (/cannot be changed here/.test(message)) return p.terminalOnly;
  return null;
}

/** Why a save failed: a sentence for the row and, if useful, the daemon's own message. */
export function saveFailure(f: ApiFailure, t: Messages): { text: string; detail: string | null } {
  const p = t.settings.problem;
  const detail = f.message && f.message !== "" ? f.message : null;
  switch (f.kind) {
    case "unauthorized":
      return { text: p.session, detail: null };
    case "daemon":
      return { text: p.daemon, detail: null };
    case "bad_request": {
      const known = problemText(f.message, t);
      return known ? { text: known, detail: null } : { text: p.other, detail };
    }
  }
  if (f.status === 403) return { text: p.forbidden, detail };
  if (/and the settings saved, but the timer could not be/.test(f.message)) return { text: p.timerHalf, detail };
  if (/cannot switch automatic backups/.test(f.message)) return { text: p.timer, detail };
  if (f.status === 500 && /config(uration|\.toml)/.test(f.message)) return { text: p.configBroken, detail };
  return { text: p.other, detail };
}
