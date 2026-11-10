// What the browser can know about a settings change (`PATCH /api/v1/config`),
// with the same rules and words as the daemon (crates/nestegg/src/settings.rs,
// decisions 0013 and 0022): the ranges, the "overdue after" cap and the
// spelling-independent checks of an exclude pattern. The daemon stays the
// authority. Whether a pattern would leave out a whole folder is decided by
// the daemon alone (it runs the backup client's matcher against the real
// accounts); the UI only shows its refusal. The dev mock uses this file too.

import type { SettingKey, SettingsChange, SettingsLimits } from "./types.ts";

export const SETTING_KEYS: readonly SettingKey[] = [
  "interval_hours",
  "overdue_after_hours",
  "excludes",
  "wake_on_lan",
  "automatic_backups",
];

/**
 * The base of the "overdue after" cap through the API: seven days (decision 0022). The real cap
 * is this or the backup interval plus `per_interval_extra` hours, whichever is larger; see
 * `overdueCap`. The form reads the numbers from `limits` in `/config`; these are the daemon's
 * values for a daemon that does not send them.
 */
export const OVERDUE_BASE_HOURS = 168;
export const OVERDUE_INTERVAL_EXTRA_HOURS = 72;

/** Limits of the daemon; `GET /config` reports them as `limits`. */
export const DEFAULT_LIMITS: SettingsLimits = {
  interval_hours: { min: 1, max: 168 },
  overdue_after_hours: {
    min: 2,
    max: OVERDUE_BASE_HOURS,
    greater_than_interval: true,
    base: OVERDUE_BASE_HOURS,
    per_interval_extra: OVERDUE_INTERVAL_EXTRA_HOURS,
  },
  excludes: { max_items: 200, max_length: 256 },
};

/**
 * The largest "overdue after" the API accepts when the rhythm is `interval` hours (the rhythm in
 * effect after the same request): the larger of `base` and `interval + per_interval_extra`, and
 * never less than the value already in the file (`file_minimum`; a larger value can only be
 * lowered). A daemon that does not send the numbers is believed on `max`.
 */
export function overdueCap(interval: number, limits: SettingsLimits = DEFAULT_LIMITS): number {
  const od = limits.overdue_after_hours;
  if (od.base === undefined || od.per_interval_extra === undefined) return od.max;
  return Math.max(od.base, interval + od.per_interval_extra, od.file_minimum ?? 0);
}

/** Keys that exist, but only root can change them in the terminal. */
const TERMINAL_ONLY = new Set([
  "backup_id",
  "sources",
  "schedule",
  "encryption",
  "pbs",
  "server",
  "port",
  "datastore",
  "namespace",
  "auth_id",
  "fingerprint",
  "wake",
  "mac",
  "broadcast",
  "timeout_seconds",
  "wake_on_lan_configured",
  "backup",
  "include_dev",
  "inventory",
  "system",
  "change_detection_mode",
  "rate_limit",
  "keep_runs",
  "overview",
  "password",
  "key",
  "editable",
  "limits",
]);

/** The current values a change is checked against. */
export interface CurrentSettings {
  interval_hours: number;
  overdue_after_hours: number;
  wake_on_lan_configured: boolean;
  /** The exclude list as it is now: patterns that are already in it are not judged again. */
  excludes?: string[];
}

// A pattern that matches every entry: only slashes, or wildcard-only
// components (`*`, `**`, `/*`, `**/*`, `?*`, ...).
export function excludesEverything(pattern: string): boolean {
  if (pattern.startsWith("!")) return false;
  return pattern
    .split("/")
    .filter((part) => part !== "")
    .every((part) => part.includes("*") && /^[*?]+$/.test(part));
}

// Control characters, and the invisible format characters that reorder or hide text (line breaks,
// zero-width and bidirectional marks); the same set as the daemon's `unprintable`.
const unprintable = /\p{Cc}|[\u061c\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]/u;

const utf8Length = (s: string) => new TextEncoder().encode(s).length;

/**
 * Problem with one exclude pattern that does not depend on the machine (English, as from the
 * daemon), or null. Which folders a pattern would leave out is not decided here but by the daemon.
 */
export function excludeProblem(pattern: string, limits: SettingsLimits = DEFAULT_LIMITS): string | null {
  if (pattern === "" || pattern === "!") return "exclude patterns must not be empty";
  if (utf8Length(pattern) > limits.excludes.max_length) return `exclude patterns may have at most ${limits.excludes.max_length} bytes`;
  if (unprintable.test(pattern))
    return `exclude pattern ${JSON.stringify(pattern)} contains a control character or an invisible format character (line break, tab, NUL, zero-width or bidirectional override, ...)`;
  if (pattern.trim() !== pattern) return `exclude pattern ${JSON.stringify(pattern)} must not start or end with spaces`;
  if (pattern.startsWith("-")) return `exclude pattern \`${pattern}\` must not start with \`-\``;
  if (excludesEverything(pattern)) return `exclude pattern \`${pattern}\` would exclude everything; the backups would be empty`;
  return null;
}

const isUint = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v) && v >= 0;

/**
 * Check a `PATCH /config` body like the daemon does. Returns the error
 * message of the 400 response, or null if the change is acceptable.
 */
export function settingsProblem(
  body: unknown,
  current: CurrentSettings,
  limits: SettingsLimits = DEFAULT_LIMITS,
  /**
   * What the daemon decides about a newly added pattern (the dev mock's stand-in for the backup
   * client's matcher; the browser passes nothing). Returns the daemon's refusal, or null.
   */
  decideNewPattern?: (pattern: string) => string | null,
): string | null {
  if (typeof body !== "object" || body === null || Array.isArray(body)) return "the request body must be a JSON object";
  const raw = body as Record<string, unknown>;
  for (const key of Object.keys(raw)) {
    if ((SETTING_KEYS as readonly string[]).includes(key)) continue;
    if (TERMINAL_ONLY.has(key))
      return `${JSON.stringify(key)} cannot be changed here; as root, edit /etc/nestegg/config.toml or use the nestegg commands in the terminal`;
    return `unknown setting ${JSON.stringify(key)}; editable settings are: ${SETTING_KEYS.join(", ")}`;
  }
  const given = (k: SettingKey) => raw[k] !== undefined && raw[k] !== null;
  for (const k of ["interval_hours", "overdue_after_hours"] as const)
    if (given(k) && !isUint(raw[k])) return `invalid settings: ${k} must be a non-negative integer`;
  for (const k of ["wake_on_lan", "automatic_backups"] as const)
    if (given(k) && typeof raw[k] !== "boolean") return `invalid settings: ${k} must be true or false`;
  if (given("excludes") && !(Array.isArray(raw.excludes) && raw.excludes.every((p) => typeof p === "string")))
    return "invalid settings: excludes must be a list of strings";
  const change = raw as SettingsChange;

  const { interval_hours: iv, overdue_after_hours: od } = limits;
  if (given("interval_hours") && (change.interval_hours! < iv.min || change.interval_hours! > iv.max))
    return `interval_hours must be between ${iv.min} and ${iv.max} (one week)`;
  if (given("overdue_after_hours")) {
    // The cap follows the rhythm in effect after this same request.
    const cap = overdueCap(change.interval_hours ?? current.interval_hours, limits);
    if (change.overdue_after_hours! < od.min || change.overdue_after_hours! > cap) {
      const base = od.base ?? OVERDUE_BASE_HOURS;
      const extra = current.overdue_after_hours > base ? `; the current value of ${current.overdue_after_hours} can only be lowered here` : "";
      return `overdue_after_hours must be between ${od.min} and ${cap} hours through the web interface${extra}. A longer time can only be set as root in /etc/nestegg/config.toml.`;
    }
  }
  if (given("interval_hours") || given("overdue_after_hours")) {
    const interval = change.interval_hours ?? current.interval_hours;
    const overdue = change.overdue_after_hours ?? current.overdue_after_hours;
    if (overdue <= interval)
      return `overdue_after_hours (${overdue}) must be greater than interval_hours (${interval}); otherwise the dashboard reports "overdue" before the next backup is due`;
  }
  if (given("excludes")) {
    const list = change.excludes!;
    if (list.length > limits.excludes.max_items) return `at most ${limits.excludes.max_items} exclude patterns are allowed`;
    const seen = new Set<string>();
    for (const p of list) {
      const problem = excludeProblem(p, limits);
      if (problem) return problem;
      if (seen.has(p)) return `exclude pattern \`${p}\` is listed twice`;
      seen.add(p);
    }
    // A pattern that is already in the list stays (an administrator may have put it there).
    if (decideNewPattern)
      for (const p of list) {
        if (current.excludes?.includes(p)) continue;
        const refused = decideNewPattern(p);
        if (refused) return refused;
      }
  }
  if (change.wake_on_lan === true && !current.wake_on_lan_configured)
    return "Wake-on-LAN is not set up: the configuration has no [wake] section with the MAC address of the backup server. Add it in the terminal (see docs/configuration.md), then switch it on here.";
  return null;
}
