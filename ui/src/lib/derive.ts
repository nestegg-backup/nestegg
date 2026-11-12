// Pure state derivation: what the UI shows for a given API state.
// No strings here except stable ids; texts live in ../i18n.

import { daysBetween, shiftDayKey, type Formatter } from "./format";
import type { ConfigView, Machine, Run, RunErrorKind, RunState, RunTrigger, Status, Target, TargetStatus } from "./types";

export const ACTIVE_STATES = ["starting", "waking_pbs", "inventory", "uploading", "finishing"] as const;

export function isActive(run: Pick<Run, "state"> | null | undefined): boolean {
  return !!run && (ACTIVE_STATES as readonly string[]).includes(run.state);
}

// ---------------------------------------------------------------- values of a newer version

// The API sends `unknown` for a value written by a newer version of nestegg (api/openapi.yaml). A
// daemon that is older than this page, or a future one, can also send a word this page has never
// seen. Such a word is the same thing as `unknown`: it must never fall into a branch that was meant
// for a known value (cancelled, succeeded, internal error, ...). Every lookup over a state, trigger
// or error kind goes through these three functions.

const RUN_STATES: readonly string[] = [...ACTIVE_STATES, "succeeded", "failed", "pbs_unreachable", "stick_not_plugged_in", "cancelled"];
const RUN_TRIGGERS: readonly string[] = ["timer", "manual", "cli", "plug"];
const ERROR_KINDS: readonly string[] = [
  "stick_not_plugged_in",
  "stick_full",
  "stick_already_mounted",
  "stick_ambiguous",
  "stick_busy",
  "filesystem_damaged",
  "stick_unsupported",
  "key_mismatch",
  "pbs_unreachable",
  "authentication",
  "source_missing",
  "client_missing",
  "client_failed",
  "already_running",
  "config",
  "credentials_unreadable",
  "verification_failed",
  "cancelled",
  "internal",
];

/** The state of a run; a state this page does not know counts as `unknown`. */
export function runState(run: Pick<Run, "state">): RunState {
  return RUN_STATES.includes(run.state) ? run.state : "unknown";
}

/** The run's state is one this version does not understand: it must be shown as exactly that. */
export function isUnknownRun(run: Pick<Run, "state"> | null | undefined): boolean {
  return !!run && runState(run) === "unknown";
}

export function runTrigger(run: Pick<Run, "trigger">): RunTrigger {
  return RUN_TRIGGERS.includes(run.trigger) ? run.trigger : "unknown";
}

/** An error kind as one this page has a text for; anything else is `unknown`. */
export function knownErrorKind(kind: string | null | undefined): RunErrorKind {
  return kind && ERROR_KINDS.includes(kind) ? (kind as RunErrorKind) : "unknown";
}

/** What the detail page of this PC is about, in priority order. */
export type SelfKind = "unconfigured" | "running" | "key_missing" | "failing" | "overdue" | "incomplete" | "never" | "ok";

export function selfKind(s: Status): SelfKind {
  if (s.health === "unconfigured") return "unconfigured";
  if (s.health === "running" || isActive(s.current_run)) return "running";
  if (s.encryption.enabled && s.encryption.key_exported === false) return "key_missing";
  // The daemon reports `failing` when the last run's state is unknown. An older daemon says `ok`
  // then: the page does not repeat "Everything is fine" above an entry it cannot read.
  if (isUnknownRun(s.last_run) && (s.health === "ok" || s.health === "incomplete" || s.health === "overdue")) return "failing";
  switch (s.health) {
    case "failing":
      return "failing";
    case "overdue":
      return "overdue";
    case "incomplete":
      return "incomplete";
    case "never":
      return "never";
    default:
      return "ok";
  }
}

/** Colour of a status dot. `neutral` is the grey mark colour. */
export type Tone = "ok" | "warn" | "bad" | "pbs" | "running" | "neutral";

export function selfTone(kind: SelfKind): Tone {
  switch (kind) {
    case "running":
      return "running";
    case "key_missing":
    case "overdue":
    case "incomplete":
      return "warn";
    case "failing":
      return "bad";
    case "ok":
      return "ok";
    default:
      return "neutral";
  }
}

/** Time a run ended, falling back to its start. */
export function runEnd(run: Run): string {
  return run.finished_at ?? run.started_at;
}

/** Whole hours and days since an instant. */
export function ageOf(iso: string, now: Date): { hours: number; days: number } {
  const ms = now.getTime() - new Date(iso).getTime();
  const hours = Math.max(0, Math.floor(ms / 3_600_000));
  return { hours, days: Math.floor(hours / 24) };
}

// ---------------------------------------------------------------- 14-day strip

export type DayKind = "ok" | "incomplete" | "damaged" | "fail" | "pbs" | "unknown" | "none" | "run";

export interface DayCell {
  key: string; // YYYY-MM-DD, local calendar day; format it with f.keyShort/keyLong/keyNumber
  kind: DayKind;
  today: boolean;
}

/** The last `count` local days (oldest first) and what happened on each. */
export function dayStrip(
  events: { at: string; kind: Exclude<DayKind, "none"> }[],
  now: Date,
  f: Formatter,
  count = 14,
): DayCell[] {
  const todayKey = f.dayKey(now);
  const byDay = new Map<string, Set<DayKind>>();
  for (const e of events) {
    const k = f.dayKey(e.at);
    if (!byDay.has(k)) byDay.set(k, new Set());
    byDay.get(k)!.add(e.kind);
  }
  // A complete backup beats an incomplete one of the same day; a day whose
  // only snapshot is damaged is not a backed-up day.
  // A run this version cannot read is the weakest evidence: any known result of the day beats it.
  const rank: DayKind[] = ["ok", "incomplete", "damaged", "run", "fail", "pbs", "unknown"];
  const cells: DayCell[] = [];
  for (let i = count - 1; i >= 0; i--) {
    const key = shiftDayKey(todayKey, -i);
    const kinds = byDay.get(key);
    const kind = rank.find((r) => kinds?.has(r)) ?? "none";
    cells.push({ key, kind, today: i === 0 });
  }
  return cells;
}

export function runEvents(
  runs: Run[],
  current?: Run | null,
  /** Snapshot paths the server marked as damaged (see `damagedSnapshots`). */
  damaged: ReadonlySet<string> = new Set(),
): { at: string; kind: Exclude<DayKind, "none"> }[] {
  const out: { at: string; kind: Exclude<DayKind, "none"> }[] = [];
  for (const r of runs) {
    const state = runState(r);
    switch (state) {
      case "succeeded": {
        const k = succeededKind(r, damaged);
        out.push({ at: r.started_at, kind: k === "warnings" ? "ok" : k });
        break;
      }
      case "failed":
        out.push({ at: r.started_at, kind: "fail" });
        break;
      case "pbs_unreachable":
        out.push({ at: r.started_at, kind: "pbs" });
        break;
      case "unknown":
        out.push({ at: r.started_at, kind: "unknown" });
        break;
      default:
        // cancelled and "stick not plugged in": no mark; a running state is drawn from `current` below
        break;
    }
  }
  if (current && isActive(current)) out.push({ at: current.started_at, kind: "run" });
  return out;
}

/**
 * Snapshots on the server as successful days of the strip. The local run
 * history is empty after a reinstall; the server still knows those days.
 */
export function snapshotEvents(
  snapshots: { time: string; snapshot?: string }[],
  damaged: ReadonlySet<string> = new Set(),
): { at: string; kind: "ok" | "damaged" }[] {
  return snapshots.map((s) => ({ at: s.time, kind: s.snapshot && damaged.has(s.snapshot) ? "damaged" : "ok" }));
}

/**
 * The 14 days of this PC: its local run history plus the snapshots on the
 * server (which also cover the days before a reinstall). The boxes on its
 * page and the mini strip in the sidebar use the same cells.
 */
export function selfStrip(
  runs: Run[],
  current: Run | null | undefined,
  snapshots: { time: string; snapshot?: string; verified?: "ok" | "failed" | null }[],
  now: Date,
  f: Formatter,
  /** `status.server_verify_failed`. */
  verifyFailed: readonly string[] = [],
): DayCell[] {
  const damaged = damagedSnapshots(snapshots, verifyFailed);
  // A snapshot made by a run in the local history is drawn from that run (it may be incomplete);
  // only the others (older installation, other source) come from the server's list.
  const known = new Set(runs.map((r) => r.snapshot).filter((x): x is string => !!x));
  const fromServer = snapshots.filter((x) => !x.snapshot || !known.has(x.snapshot));
  return dayStrip([...runEvents(runs, current, damaged), ...snapshotEvents(fromServer, damaged)], now, f);
}

// ---------------------------------------------------------------- incomplete and damaged backups

/** A successful run that does not contain everything the configuration asks for. */
export function isIncomplete(run: Run): boolean {
  return run.state === "succeeded" && (run.incomplete?.length ?? 0) > 0;
}

/**
 * What a successful run is, for its row in the history and its box in the
 * 14-day strip alike (they must agree): damaged on the server beats
 * incomplete beats "with notes" beats plain ok.
 */
export function succeededKind(run: Run, damaged: ReadonlySet<string> = new Set()): "damaged" | "incomplete" | "warnings" | "ok" {
  if (run.snapshot && damaged.has(run.snapshot)) return "damaged";
  if (isIncomplete(run)) return "incomplete";
  return otherWarnings(run).length ? "warnings" : "ok";
}

/** Warnings of a run without the ones that are already listed as reasons for "incomplete". */
export function otherWarnings(run: Run): string[] {
  const reasons = new Set(run.incomplete ?? []);
  return (run.warnings ?? []).filter((w) => !reasons.has(w));
}

/**
 * Snapshot paths (`host/desktop/2026-11-08T21:30:12Z`) known to be damaged:
 * the ones the server's own check named, plus snapshots whose own
 * verification state is "failed".
 */
export function damagedSnapshots(
  snapshots: { snapshot?: string; verified?: "ok" | "failed" | null }[],
  verifyFailed: readonly string[] = [],
): Set<string> {
  const out = new Set(verifyFailed);
  for (const s of snapshots) if (s.verified === "failed" && s.snapshot) out.add(s.snapshot);
  return out;
}

export function isDamaged(snapshot: { snapshot?: string; verified?: "ok" | "failed" | null }, verifyFailed: readonly string[] = []): boolean {
  return snapshot.verified === "failed" || (!!snapshot.snapshot && verifyFailed.includes(snapshot.snapshot));
}

/** Every snapshot is damaged: then "pick another one" is no advice. False for an empty list. */
export function allDamaged(snapshots: { snapshot?: string; verified?: "ok" | "failed" | null }[], verifyFailed: readonly string[] = []): boolean {
  return snapshots.length > 0 && snapshots.every((s) => isDamaged(s, verifyFailed));
}

/** Time part of a snapshot path (`host/desktop/2026-11-08T21:30:12Z` -> `2026-11-08T21:30:12Z`). */
export function snapshotTimeOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** The days (newest first, as instants) of the damaged snapshots named by the server. */
export function damagedTimes(verifyFailed: readonly string[] | undefined, snapshots: { time: string; snapshot?: string; verified?: "ok" | "failed" | null }[] = []): string[] {
  const times = new Set((verifyFailed ?? []).map(snapshotTimeOf).filter((x) => !Number.isNaN(Date.parse(x))));
  for (const s of snapshots) if (s.verified === "failed") times.add(s.time);
  return [...times].sort().reverse();
}

// ---------------------------------------------------------------- run history

export interface RunRow {
  run: Run; // the newest run of the group
  count: number; // > 1 when consecutive "server not reachable" attempts on one day were folded
  expandable: boolean;
}

/**
 * Finished runs, newest first. Consecutive `pbs_unreachable` attempts on the
 * same day (the timer retries hourly) are folded into one row.
 */
export function historyRows(runs: Run[], f: Formatter): RunRow[] {
  const rows: RunRow[] = [];
  for (const run of runs) {
    if (isActive(run)) continue;
    const prev = rows[rows.length - 1];
    // The same for a stick that was not plugged in: the timer looks for it every hour.
    if (
      prev &&
      (run.state === "pbs_unreachable" || run.state === "stick_not_plugged_in") &&
      prev.run.state === run.state &&
      f.dayKey(prev.run.started_at) === f.dayKey(run.started_at)
    ) {
      prev.count++;
      continue;
    }
    const expandable =
      run.state === "failed" ||
      run.state === "pbs_unreachable" ||
      isUnknownRun(run) ||
      (run.warnings?.length ?? 0) > 0 ||
      isIncomplete(run);
    rows.push({ run, count: 1, expandable });
  }
  return rows;
}

/** Colour of the dot in a history row. A run this version cannot read is amber: neither a success nor a failure. */
export function runTone(run: Run, damaged: ReadonlySet<string> = new Set()): Tone {
  switch (runState(run)) {
    case "failed":
      return "bad";
    case "pbs_unreachable":
      return "pbs";
    case "succeeded": {
      const k = succeededKind(run, damaged);
      return k === "damaged" ? "bad" : k === "incomplete" || k === "warnings" ? "warn" : "neutral";
    }
    case "unknown":
      return "warn";
    default:
      return "neutral";
  }
}

export function errorKind(run: Run): RunErrorKind | null {
  if (run.error?.kind) return knownErrorKind(run.error.kind);
  switch (runState(run)) {
    case "pbs_unreachable":
      return "pbs_unreachable";
    case "failed":
      return "internal";
    case "unknown":
      return "unknown";
    default:
      return null;
  }
}

/** Progress steps for a running backup: index of the current step (0-3). */
export function runStep(state: Run["state"]): number {
  switch (runState({ state })) {
    case "starting":
    case "waking_pbs":
      return 0;
    case "inventory":
      return 1;
    case "uploading":
      return 2;
    case "finishing":
      return 3;
    default:
      return 4;
  }
}

// ---------------------------------------------------------------- server contact

/**
 * What a finished run proves about the backup server. The run pipeline checks
 * the key and the source folders before it contacts the PBS, so `config`,
 * `source_missing` and `already_running` failures say nothing about the
 * server. `credentials_unreadable` is a local problem that can show up
 * before or after the availability check, so it proves nothing either.
 * `authentication`, `client_missing` and `client_failed` happen in the
 * backup step, after the PBS answered the availability check. `internal` can
 * happen anywhere.
 */
export function serverContact(run: Run): "reached" | "not_reached" | "unknown" {
  // A run to the stick says nothing about the backup server.
  if (run.target === "stick") return "unknown";
  const state = runState(run);
  if (state === "succeeded") return "reached";
  if (state === "pbs_unreachable") return "not_reached";
  if (state === "failed") {
    const k = errorKind(run);
    if (k === "authentication" || k === "client_missing" || k === "client_failed") return "reached";
    if (k === "pbs_unreachable") return "not_reached";
  }
  return "unknown";
}

// ---------------------------------------------------------------- other machines

export type MachineKind = "failing" | "key_missing" | "overdue" | "never" | "ok";

export function machineKind(m: Machine, now: Date, fallbackOverdueHours = 36): MachineKind {
  const last = m.last_snapshot ?? null;
  const report = m.report ?? null;
  if (report?.result === "failed") {
    const failedAt = report.run_started_at ?? report.reported_at;
    if (!last || new Date(failedAt).getTime() > new Date(last).getTime()) return "failing";
  }
  if (!last) return "never";
  const threshold = report?.overdue_after_hours ?? fallbackOverdueHours;
  if (now.getTime() - new Date(last).getTime() > threshold * 3_600_000) return "overdue";
  if (report?.key_exported === false) return "key_missing";
  return "ok";
}

export function machineTone(kind: MachineKind): Tone {
  switch (kind) {
    case "failing":
      return "bad";
    case "overdue":
    case "key_missing":
      return "warn";
    case "ok":
      return "ok";
    default:
      return "neutral";
  }
}

const MACHINE_ORDER: MachineKind[] = ["failing", "key_missing", "overdue", "never", "ok"];

/** Problems first, then by name. Excludes this PC. */
export function sortMachines(machines: Machine[], selfBackupId: string | null, now: Date, fallbackOverdueHours?: number): Machine[] {
  return machines
    .filter((m) => m.backup_id !== selfBackupId)
    .map((m) => ({ m, k: MACHINE_ORDER.indexOf(machineKind(m, now, fallbackOverdueHours)) }))
    .sort((a, b) => a.k - b.k || machineName(a.m).localeCompare(machineName(b.m)))
    .map((x) => x.m);
}

export function machineName(m: Machine): string {
  return m.hostname || m.backup_id;
}

export function machineEvents(m: Machine): { at: string; kind: Exclude<DayKind, "none"> }[] {
  const out: { at: string; kind: Exclude<DayKind, "none"> }[] = (m.recent_snapshots ?? []).map((at) => ({ at, kind: "ok" }));
  if (m.last_snapshot && !(m.recent_snapshots ?? []).includes(m.last_snapshot)) out.push({ at: m.last_snapshot, kind: "ok" });
  const r = m.report;
  if (r && r.result !== "succeeded") {
    const kind = r.result === "failed" ? "fail" : r.result === "pbs_unreachable" ? "pbs" : "unknown";
    out.push({ at: r.run_started_at ?? r.reported_at, kind });
  }
  return out;
}

/**
 * Days with a snapshot of another PC, for its mini strip in the sidebar.
 * The PBS only knows successful backups (a failed run leaves no snapshot),
 * so these strips tell "backed up" from "no backup" and nothing more.
 */
export function machineSnapshotEvents(m: Machine): { at: string; kind: "ok" }[] {
  const times = [...(m.recent_snapshots ?? [])];
  if (m.last_snapshot && !times.includes(m.last_snapshot)) times.push(m.last_snapshot);
  return times.map((at) => ({ at, kind: "ok" }));
}

// ---------------------------------------------------------------- backups on the server

/** What the PBS holds for this PC, from `/snapshots`. */
export interface ServerBackups {
  count: number;
  /** Time of the newest snapshot (its start time), null when there is none. */
  latest: string | null;
  /** Snapshots whose own verification state is "failed". */
  damaged?: number;
}

type SnapshotState = { state: "idle" | "loading" | "ok" | "error"; list: { time: string; verified?: "ok" | "failed" | null }[] };

/** Only a loaded list says something; loading or failed lists are no evidence either way. */
export function serverBackups(snaps: SnapshotState): ServerBackups | null {
  if (snaps.state !== "ok") return null;
  const latest = snaps.list.reduce<string | null>((a, s) => (a === null || s.time > a ? s.time : a), null);
  return { count: snaps.list.length, latest, damaged: snaps.list.filter((x) => x.verified === "failed").length };
}

/**
 * Whether "Restore files" is offered on the page of this PC. It depends on
 * what the server holds, never on the local run history: after a reinstall
 * the local state is empty, but the snapshots on the PBS are still there.
 * While the list is loading, or when loading failed (the restore flow shows
 * the error and a retry), the entry stays visible. Only a loaded, empty list
 * hides it (`empty`).
 */
export function restoreOffer(kind: SelfKind, snaps: SnapshotState): "show" | "empty" | "none" {
  if (kind === "unconfigured") return "none";
  if (snaps.state === "ok" && !snaps.list.length) return "empty";
  return "show";
}

// ---------------------------------------------------------------- misc

/** "every 24 h" / "every 24 h (checked hourly)" -> 24. Only for daemons that predate `interval_hours`. */
export function intervalHours(schedule: string | undefined | null): number | null {
  const m = /every\s+(\d+)\s*h/i.exec(schedule ?? "");
  return m ? Number(m[1]) : null;
}

/**
 * How often a backup is due, in hours. The structured `interval_hours` of the
 * configuration or the status wins; the text `schedule` is only parsed for
 * daemons that do not send the number.
 */
export function scheduleHours(config: ConfigView | null | undefined, status: Status | null | undefined): number | null {
  return (
    config?.interval_hours ??
    status?.schedule.interval_hours ??
    intervalHours(config?.schedule ?? status?.schedule.calendar) ??
    null
  );
}

/** Archive file names of a snapshot (`home.pxar.didx`) to restorable archives (`home.pxar`). */
export function restorableArchives(files: string[] | undefined): string[] {
  return (files ?? []).filter((a) => a.endsWith(".pxar.didx")).map((a) => a.slice(0, -".didx".length));
}

export function isInventoryArchive(archive: string): boolean {
  return archive === "inventory.pxar";
}

/** Days between two instants in local calendar days (b - a). */
export function calendarDays(a: string | Date, b: string | Date, f: Formatter): number {
  return daysBetween(f.dayKey(a), f.dayKey(b));
}

// ---------------------------------------------------------------- setup and key guide

export type GuideStep = "connect" | "credentials" | "key_create" | "key_store" | "automatic" | "first_backup";

export interface Guide {
  /** Steps in order; the key steps are left out when encryption is switched off. */
  steps: GuideStep[];
  current: GuideStep;
}

/**
 * Whether the encryption key still has to be created. `key_present` says it
 * directly. A daemon that does not send it (null or absent) leaves only the
 * old rule: no fingerprint and no confirmed copy. A key that exists but has
 * no fingerprint must never lead to "create the key": `key create` refuses
 * to replace a key, and only the destructive `--force` would work.
 */
export function keyMissing(s: Status): boolean {
  const e = s.encryption;
  if (typeof e.key_present === "boolean") return !e.key_present;
  return e.key_fingerprint === null && e.key_exported === false;
}

/**
 * Why no backup can run at the moment (so "next backup: 02:00" would be untrue), or null.
 * Same order as the setup guide: the token secret, then the key, then the key copy. Unknown
 * values (null or absent) never count as missing.
 */
export type Blocked = "unconfigured" | "credentials" | "credentials_unreadable" | "key_create" | "key_confirm";

export function backupBlocked(s: Status): Blocked | null {
  if (s.health === "unconfigured") return "unconfigured";
  // Without a backup server there is no token secret to store (stick-only setups).
  if (hasPbs(s) && s.pbs.credentials_stored === false) return "credentials";
  // The stored secrets exist, but cannot be decrypted (new mainboard): every run fails the same way
  // until they are stored again. A backup that is running now shows that it got further.
  if (s.last_run?.state === "failed" && s.last_run.error?.kind === "credentials_unreadable" && !isActive(s.current_run))
    return "credentials_unreadable";
  if (s.encryption.enabled && keyMissing(s)) return "key_create";
  if (s.encryption.enabled && s.encryption.key_exported === false) return "key_confirm";
  return null;
}

/**
 * Where the user is on the way from a fresh install to the first backup, or
 * null once there is nothing (more) to guide. Setup and key work happen in a
 * terminal as root; the page only tells which step is next and moves on by
 * itself when /status shows the step as done:
 *
 * 1. connect: `nestegg init` has not run (`health = unconfigured`)
 * 2. credentials: the token secret is not stored (`pbs.credentials_stored === false`)
 * 3. key_create: encryption is on and there is no key
 * 4. key_store: the key exists, but no copy outside this PC is confirmed
 * 5. automatic: the timer is off (`schedule.enabled === false`). Only while
 *    the setup is not finished: a PC with a backup and another step still open,
 *    or without any backup. Someone who switched the timer off on purpose
 *    is not nagged forever.
 * 6. first_backup: configured, but no backup yet
 *
 * Unknown values (null or absent) never count as "missing".
 */
export function setupGuide(s: Status): Guide | null {
  if (s.health === "running" || isActive(s.current_run)) return null;
  const unconfigured = s.health === "unconfigured";
  // Before setup it is not known whether encryption will be used: the default is yes.
  const encrypted = unconfigured || s.encryption.enabled;
  const noKey = encrypted && keyMissing(s);

  const open: Record<Exclude<GuideStep, "automatic">, boolean> = {
    connect: unconfigured,
    credentials: !unconfigured && hasPbs(s) && s.pbs.credentials_stored === false,
    key_create: !unconfigured && noKey,
    key_store: !unconfigured && encrypted && !noKey && s.encryption.key_exported === false,
    first_backup: s.health === "never",
  };
  const anyOther = Object.values(open).some(Boolean);
  const automatic = !unconfigured && s.schedule.enabled === false && (anyOther || !s.last_success);

  const steps: GuideStep[] = unconfigured || hasPbs(s) ? ["connect", "credentials"] : [];
  if (encrypted) steps.push("key_create", "key_store");
  steps.push("automatic");
  // A backup that exists already makes "first backup" meaningless.
  if (!s.last_success) steps.push("first_backup");

  const order: GuideStep[] = ["connect", "credentials", "key_create", "key_store", "automatic", "first_backup"];
  const current = order.find((step) => (step === "automatic" ? automatic : open[step]));
  if (!current || !steps.includes(current)) return null;
  return { steps, current };
}

// ---------------------------------------------------------------- commands shown to the user

/**
 * A command is only complete when it contains no placeholder. Placeholders
 * are written as `<NAME>` without quotes: a shell stops at them with a
 * syntax error or a missing file, so a command copied by hand fails instead
 * of doing something with the placeholder as its value.
 */
export function hasPlaceholder(command: string): boolean {
  return /<[A-Z][A-Z0-9-]*>/.test(command);
}

/** Single-quote a value for a POSIX shell when it needs it (the token ID contains `!`, an IPv6 address `[`). */
export function shellQuote(value: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : `'${value.replace(/'/g, `'\\''`)}'`;
}

/** `--name=value`: in this form a value that starts with `-` can never be read as an option. */
function option(name: string, value: string | null, placeholder: string): string {
  return `--${name}=${value ? shellQuote(value) : `<${placeholder}>`}`;
}

/**
 * The address field as `nestegg init` wants it: a host name or IP address
 * without a port, IPv6 in square brackets, and the port on its own. Accepts
 * `host`, `host:port`, `[v6]`, `[v6]:port` and a bare IPv6 address.
 */
export function splitServer(input: string): { server: string; port: string | null } {
  const v = input.trim();
  const port = (p: string | undefined) => (p && Number(p) >= 1 && Number(p) <= 65535 ? p : null);
  let m = /^\[([^\]]+)\](?::(\d{1,5}))?$/.exec(v);
  if (m && (m[2] === undefined || port(m[2]))) return { server: `[${m[1]}]`, port: port(m[2]) };
  m = /^([^:[\]]+):(\d{1,5})$/.exec(v);
  if (m && port(m[2])) return { server: m[1]!, port: m[2]! };
  // Several colons and no brackets: a bare IPv6 address, which init only takes in brackets.
  if (/^[0-9A-Fa-f:.]+$/.test(v) && v.split(":").length > 2) return { server: `[${v}]`, port: null };
  return { server: v, port: null };
}

export interface InitFields {
  server: string;
  datastore: string;
  namespace: string;
  authId: string;
}

/**
 * The `nestegg init` command for the entered values. Empty required fields
 * show as `<NAME>` (see `hasPlaceholder`: such a command cannot be copied).
 * Never contains a secret: the token secret is entered in the terminal.
 */
export function initCommand(v: InitFields): string {
  const { server, port } = splitServer(v.server);
  const options = [
    option("server", server, "SERVER"),
    ...(port ? [option("port", port, "PORT")] : []),
    option("datastore", v.datastore.trim(), "DATASTORE"),
    option("auth-id", v.authId.trim(), "TOKEN-ID"),
  ];
  if (v.namespace.trim()) options.push(option("namespace", v.namespace.trim(), "NAMESPACE"));
  // One option per line (with a trailing backslash) so the command stays readable; it still pastes as one command.
  return ["sudo nestegg init", ...options].join(" \\\n  ");
}

/** File name of the key on a USB stick. */
export const KEY_FILE_NAME = "nestegg-key.json";

/** Full path of the key file for the entered stick path; null unless it is an absolute path. */
export function stickFile(stickPath: string): string | null {
  const dir = stickPath.trim().replace(/\/+$/, "");
  return dir.startsWith("/") ? `${dir}/${KEY_FILE_NAME}` : null;
}

export function keyExportCommand(stickPath: string): string {
  const file = stickFile(stickPath);
  return `sudo nestegg key export ${file ? shellQuote(file) : "<STICK-PATH>"}`;
}

/** A copy on a USB stick needs no `--accept-local-copy`; a copy on this computer's own disk would. */
export function keyConfirmFileCommand(stickPath: string): string {
  const file = stickFile(stickPath);
  return `sudo nestegg key confirm-exported \\\n  ${option("file", file, "STICK-PATH")}`;
}

/** The check code of a paper key as `ABCD-0123` (8 hex digits; case, spaces and the hyphen do not matter); null otherwise. */
export function normalizeCheckCode(input: string): string | null {
  const digits = input.replace(/[\s-]/g, "").toUpperCase();
  return /^[0-9A-F]{8}$/.test(digits) ? `${digits.slice(0, 4)}-${digits.slice(4)}` : null;
}

export function keyConfirmCodeCommand(code: string): string {
  return `sudo nestegg key confirm-exported \\\n  ${option("paper-code", normalizeCheckCode(code), "CODE")}`;
}

/** Where the guide writes the paper key file; the delete command below names the same path. */
const PAPER_KEY_FILE = '"$HOME/nestegg-paper-key.html"';

/** The paper key as an HTML page to print. `--format=html` is needed, the default is plain text. */
export const PAPER_KEY_COMMAND = `sudo nestegg key paperkey \\\n  --format=html \\\n  --output=${PAPER_KEY_FILE}`;

/**
 * Deletes the paper key file after printing. nestegg remembers where it wrote the file and refuses
 * to confirm the paper key while it still lies on this computer (decision 0018).
 */
export const PAPER_KEY_DELETE_COMMAND = `rm ${PAPER_KEY_FILE}`;

export const SET_PASSWORD_COMMAND = "sudo nestegg credentials set-password";
export const CHECK_COMMAND = "sudo nestegg check";
export const KEY_CREATE_COMMAND = "sudo nestegg key create";
export const ENABLE_TIMERS_COMMAND = "sudo systemctl enable --now nestegg-backup.timer nestegg-notify.timer";

// ---------------------------------------------------------------- targets: backup server and stick

/**
 * A backup server is configured. Older daemons send no `targets` (they only know the server); an
 * unconfigured daemon sends an empty object. Only a setup whose sole target is the stick has none.
 */
export function hasPbs(s: Status): boolean {
  return !s.targets || !!s.targets.pbs || !s.targets.stick;
}

export function hasStick(s: Status): boolean {
  return !!s.targets?.stick;
}

/** The stick is the only target: there is no server, no snapshot list and no restore through the server. */
export function stickOnly(s: Status): boolean {
  return hasStick(s) && !s.targets?.pbs;
}

/** The target the legacy fields of `/status` describe: the server when there is one, otherwise the stick. */
export function primaryTarget(s: Status): Target {
  return hasPbs(s) ? "pbs" : "stick";
}

/** The target of a run; records of older versions have none and belong to the server. */
export function runTarget(run: Pick<Run, "target">): Target {
  return run.target === "stick" ? "stick" : "pbs";
}

/** What the stick row says, in priority order. */
export type StickKind =
  | "running"
  | "full"
  | "damaged"
  | "verify_failed"
  | "store_damaged"
  | "key_mismatch"
  | "unsupported"
  | "failing"
  | "unknown"
  | "overdue"
  | "incomplete"
  | "never"
  | "ok";

export interface StickView {
  kind: StickKind;
  /**
   * Live registered-stick presence. Older daemons fall back to target state;
   * an explicit null stays unknown.
   */
  plugged: boolean | null;
  /**
   * The last stick backup ended successfully, so the daemon had stopped its server, flushed and
   * unmounted the stick, no run has started since, and live usage explicitly says idle.
   */
  safeToUnplug: boolean;
  running: Run | null;
  /** Snapshots on the stick that its own verification marked as damaged. */
  damaged: number;
}

export function stickView(ts: TargetStatus): StickView {
  const running = isActive(ts.current_run) ? ts.current_run! : null;
  const last = ts.last_run ?? null;
  const plugged = "plugged_in" in ts ? ts.plugged_in ?? null
    : running ? true : ts.state === "stick_not_plugged_in" ? false : ts.state === "stick_full" ? null : true;
  const damaged = ts.server_verify_failed?.length ?? 0;
  const kind = ((): StickKind => {
    if (running || ts.health === "running") return "running";
    if (ts.state === "stick_full" || (last?.state === "failed" && last.error?.kind === "stick_full")) return "full";
    if (ts.state === "stick_damaged") return "store_damaged";
    if (isUnknownRun(last)) return "unknown";
    if (last?.state === "failed" || ts.health === "failing") {
      switch (last?.state === "failed" ? knownErrorKind(last.error?.kind) : null) {
        case "filesystem_damaged":
          return "damaged";
        case "verification_failed":
          return "verify_failed";
        case "key_mismatch":
          return "key_mismatch";
        case "stick_unsupported":
          return "unsupported";
        default:
          return "failing";
      }
    }
    switch (ts.health) {
      case "overdue":
        return "overdue";
      case "incomplete":
        return "incomplete";
      case "never":
        return "never";
      default:
        return "ok";
    }
  })();
  return {
    kind,
    plugged,
    safeToUnplug: !running && ts.in_use === null && !Object.values(ts.sticks ?? {}).some((s) => s.in_use) && plugged === true && last?.state === "succeeded",
    running,
    damaged,
  };
}

export function stickTone(kind: StickKind): Tone {
  switch (kind) {
    case "running":
      return "running";
    case "full":
    case "damaged":
    case "store_damaged":
    case "verify_failed":
    case "key_mismatch":
    case "unsupported":
    case "failing":
      return "bad";
    case "unknown":
    case "overdue":
    case "incomplete":
      return "warn";
    case "ok":
      return "ok";
    default:
      return "neutral";
  }
}

/**
 * "Back up to stick now" is offered only when a registered stick is plugged in and no backup runs
 * (the run lock is shared by both targets). A full stick is offered too: whether space was freed
 * meanwhile only shows by trying.
 */
export function canBackUpToStick(view: StickView, anyRunning: boolean): boolean {
  return !anyRunning && view.plugged !== false && view.kind !== "running";
}

/** The physical stick of the newest stick run that names one (for the commands that need `--uuid`). */
export function knownStickUuid(runs: readonly Run[], ts: TargetStatus | null | undefined, registered?: readonly string[]): string | null {
  const uuids = registered?.filter(isUuid).map((u) => u.toLowerCase());
  const reminder = uuids?.find((u) => ts?.password_reminders?.[u]);
  if (reminder) return reminder;
  for (const r of [ts?.current_run, ts?.last_run, ts?.last_success, ...runs]) {
    if (r && runTarget(r) === "stick" && r.stick_uuid && isUuid(r.stick_uuid)
      && (!uuids || uuids.includes(r.stick_uuid.toLowerCase()))) return r.stick_uuid.toLowerCase();
  }
  return uuids?.[0] ?? null;
}

/** Capacity is measured by the daemon, never inferred from uploaded bytes. */
export function stickFillPercent(ts: TargetStatus): number | null {
  const total = ts.total_bytes, free = ts.available_bytes;
  if (total == null || free == null || !Number.isFinite(total) || !Number.isFinite(free) || total <= 0 || free < 0) return null;
  return Math.round(Math.max(0, Math.min(1, 1 - free / total)) * 100);
}

export function isUuid(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value.trim());
}

const uuidOption = (uuid: string) => `--uuid=${isUuid(uuid) ? uuid.trim().toLowerCase() : "<UUID>"}`;

/** Shows the UUID of the one registered stick that is plugged in. */
export const STICK_INFO_COMMAND = "sudo nestegg stick info";

/** Makes the password-protected key copy on the stick (stick-only setups: counts as the key proof). */
export function stickKeyCopyCommand(uuid: string): string {
  return `sudo nestegg stick key-copy ${uuidOption(uuid)}`;
}

/** Opens the stick's key copy with the password again and renews the 180-day reminder. */
export function stickKeyCheckCommand(uuid: string): string {
  return `sudo nestegg stick key-check ${uuidOption(uuid)}`;
}
