// Validation of settings changes (`PATCH /api/v1/config`), the same rules as
// the daemon (crates/nestegg/src/settings.rs, decision 0013). The daemon
// stays the authority; the UI can use this to explain a problem before
// sending, and the dev mock uses it to answer like the daemon.

import type { SettingKey, SettingsChange, SettingsLimits } from "./types.ts";

export const SETTING_KEYS: readonly SettingKey[] = [
  "interval_hours",
  "overdue_after_hours",
  "excludes",
  "wake_on_lan",
  "automatic_backups",
];

/**
 * Largest "overdue after" the API accepts: seven days (decision 0022). A larger value that an
 * administrator put in the file stays and can only be lowered; `limits` then reports it as the
 * maximum. The form reads its limits from `/config`; this constant is the daemon's number for
 * the default and for the text of its refusal.
 */
export const OVERDUE_API_MAX_HOURS = 168;

/** Limits of the daemon; `GET /config` reports them as `limits`. */
export const DEFAULT_LIMITS: SettingsLimits = {
  interval_hours: { min: 1, max: 168 },
  overdue_after_hours: { min: 2, max: OVERDUE_API_MAX_HOURS, greater_than_interval: true },
  excludes: { max_items: 200, max_length: 256 },
};

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
  /** Paths of the backed-up folders (for the rule about excluding a whole folder). */
  sources?: string[];
  /** The exclude list as it is now: patterns that are already in it are not checked again. */
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

const hasControl = (s: string) => [...s].some((c) => /\p{Cc}/u.test(c));

/** Problem with one exclude pattern (English, as from the daemon), or null. */
export function excludeProblem(pattern: string, limits: SettingsLimits = DEFAULT_LIMITS): string | null {
  if (pattern === "" || pattern === "!") return "exclude patterns must not be empty";
  if ([...pattern].length > limits.excludes.max_length)
    return `exclude patterns may have at most ${limits.excludes.max_length} characters`;
  if (hasControl(pattern))
    return `exclude pattern ${JSON.stringify(pattern)} contains a control character (line break, tab, NUL, ...)`;
  if (pattern.trim() !== pattern) return `exclude pattern ${JSON.stringify(pattern)} must not start or end with spaces`;
  if (pattern.startsWith("-")) return `exclude pattern \`${pattern}\` must not start with \`-\``;
  if (excludesEverything(pattern)) return `exclude pattern \`${pattern}\` would exclude everything; the backups would be empty`;
  return null;
}

const hasWildcard = (part: string) => /[*?]/.test(part);
const isAllWildcards = (part: string) => part !== "" && /^[*?]+$/.test(part);
const trimSlashes = (path: string) => (path.length > 1 ? path.replace(/\/+$/, "") : path);

/**
 * Whether a newly added exclude would drop a whole backed-up folder or a whole top-level
 * directory (decision 0022). Same rule and same words as the daemon; English message, or null.
 * `!` patterns widen the backup and always pass.
 */
export function wholeSourceProblem(pattern: string, sources: readonly string[]): string | null {
  if (pattern.startsWith("!")) return null;
  const advice = "; an administrator can still set it as root in /etc/nestegg/config.toml";
  const anchored = pattern.startsWith("/");
  const parts = pattern.split("/").filter((p) => p !== "");
  const cut = parts.findIndex(hasWildcard);
  const fixed = cut === -1 ? parts : parts.slice(0, cut);
  const onlyWildcardsAfter = (cut === -1 ? [] : parts.slice(cut)).every(isAllWildcards);
  const paths = sources.map(trimSlashes);
  if (!anchored && fixed.length === 1 && onlyWildcardsAfter && !hasWildcard(pattern)) {
    const name = fixed[0]!;
    if (paths.some((p) => p.split("/").filter((x) => x !== "").pop() === name))
      return `exclude pattern \`${pattern}\` matches the whole backup source \`${name}\` in every archive, which would leave it out of the backup${advice}.`;
    return null;
  }
  if (anchored && fixed.length > 0 && onlyWildcardsAfter) {
    if (fixed.length === 1)
      return `exclude pattern \`${pattern}\` would drop the whole top-level directory \`/${fixed[0]}\` from every archive that contains it${advice}.`;
    const fixedPath = `/${fixed.join("/")}`;
    if (paths.some((p) => p === fixedPath || p.startsWith(`${fixedPath}/`)))
      return `exclude pattern \`${pattern}\` would drop the whole backup source \`${fixedPath}\`${advice}.`;
  }
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
  if (given("overdue_after_hours") && (change.overdue_after_hours! < od.min || change.overdue_after_hours! > od.max)) {
    // As the daemon says it: the cap is 7 days; a larger value from the file can only be lowered.
    const extra =
      current.overdue_after_hours > OVERDUE_API_MAX_HOURS
        ? `; the current value of ${current.overdue_after_hours} can only be lowered here`
        : "";
    return `overdue_after_hours must be between ${od.min} and ${OVERDUE_API_MAX_HOURS} hours (7 days) through the web interface${extra}. A longer time can only be set as root in /etc/nestegg/config.toml.`;
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
    for (const p of list) {
      if (current.excludes?.includes(p)) continue;
      const whole = wholeSourceProblem(p, current.sources ?? []);
      if (whole) return whole;
    }
  }
  if (change.wake_on_lan === true && !current.wake_on_lan_configured)
    return "Wake-on-LAN is not set up: the configuration has no [wake] section with the MAC address of the backup server. Add it in the terminal (see docs/configuration.md), then switch it on here.";
  return null;
}
