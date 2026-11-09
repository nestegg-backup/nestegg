// Pure state derivation: what the UI shows for a given API state.
// No strings here except stable ids; texts live in ../i18n.

import { daysBetween, shiftDayKey, type Formatter } from "./format";
import type { ConfigView, Machine, Run, RunErrorKind, Status } from "./types";

export const ACTIVE_STATES = ["starting", "waking_pbs", "inventory", "uploading", "finishing"] as const;

export function isActive(run: Pick<Run, "state"> | null | undefined): boolean {
  return !!run && (ACTIVE_STATES as readonly string[]).includes(run.state);
}

/** What the detail page of this PC is about, in priority order. */
export type SelfKind = "unconfigured" | "running" | "key_missing" | "failing" | "overdue" | "incomplete" | "never" | "ok";

export function selfKind(s: Status): SelfKind {
  if (s.health === "unconfigured") return "unconfigured";
  if (s.health === "running" || isActive(s.current_run)) return "running";
  if (s.encryption.enabled && s.encryption.key_exported === false) return "key_missing";
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

export type DayKind = "ok" | "incomplete" | "damaged" | "fail" | "pbs" | "none" | "run";

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
  const rank: DayKind[] = ["ok", "incomplete", "damaged", "run", "fail", "pbs"];
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
    if (r.state === "succeeded") {
      const kind = r.snapshot && damaged.has(r.snapshot) ? "damaged" : isIncomplete(r) ? "incomplete" : "ok";
      out.push({ at: r.started_at, kind });
    }
    else if (r.state === "failed") out.push({ at: r.started_at, kind: "fail" });
    else if (r.state === "pbs_unreachable") out.push({ at: r.started_at, kind: "pbs" });
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
    if (
      prev &&
      run.state === "pbs_unreachable" &&
      prev.run.state === "pbs_unreachable" &&
      f.dayKey(prev.run.started_at) === f.dayKey(run.started_at)
    ) {
      prev.count++;
      continue;
    }
    const expandable =
      run.state === "failed" || run.state === "pbs_unreachable" || (run.warnings?.length ?? 0) > 0 || isIncomplete(run);
    rows.push({ run, count: 1, expandable });
  }
  return rows;
}

export function errorKind(run: Run): RunErrorKind | null {
  if (run.error?.kind) return run.error.kind;
  if (run.state === "pbs_unreachable") return "pbs_unreachable";
  if (run.state === "failed") return "internal";
  return null;
}

/** Progress steps for a running backup: index of the current step (0-3). */
export function runStep(state: Run["state"]): number {
  switch (state) {
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
  if (run.state === "succeeded") return "reached";
  if (run.state === "pbs_unreachable") return "not_reached";
  if (run.state === "failed") {
    const k = run.error?.kind;
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
    out.push({ at: r.run_started_at ?? r.reported_at, kind: r.result === "failed" ? "fail" : "pbs" });
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
}

type SnapshotState = { state: "idle" | "loading" | "ok" | "error"; list: { time: string }[] };

/** Only a loaded list says something; loading or failed lists are no evidence either way. */
export function serverBackups(snaps: SnapshotState): ServerBackups | null {
  if (snaps.state !== "ok") return null;
  const latest = snaps.list.reduce<string | null>((a, s) => (a === null || s.time > a ? s.time : a), null);
  return { count: snaps.list.length, latest };
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

export type GuideStep = "setup" | "key_create" | "key_store" | "first_backup";

export interface Guide {
  /** Steps in order; the key steps are left out when encryption is switched off. */
  steps: GuideStep[];
  current: GuideStep;
}

/**
 * Where the user is on the way from a fresh install to the first backup, or
 * null once there is nothing (more) to guide. Setup and key work happen in a
 * terminal as root; the page only tells which step is next and moves on by
 * itself when /status shows the step as done.
 */
export function setupGuide(s: Status): Guide | null {
  if (s.health === "running" || isActive(s.current_run)) return null;
  // Before setup it is not known whether encryption will be used: the default is yes.
  const encrypted = s.health === "unconfigured" || s.encryption.enabled;
  const steps: GuideStep[] = ["setup"];
  if (encrypted) steps.push("key_create", "key_store");
  // A backup that exists already makes "first backup" meaningless.
  if (!s.last_success) steps.push("first_backup");

  let current: GuideStep | null = null;
  if (s.health === "unconfigured") current = "setup";
  // The daemon reports key_exported=false both before a key exists and while its copy is unconfirmed;
  // no fingerprint tells the two apart.
  else if (encrypted && s.encryption.key_exported === false) current = s.encryption.key_fingerprint === null ? "key_create" : "key_store";
  else if (s.health === "never") current = "first_backup";
  if (!current || !steps.includes(current)) return null;
  return { steps, current };
}

/** Single-quote a value for a POSIX shell when it needs it (the token ID contains `!`). */
export function shellQuote(value: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : `'${value.replace(/'/g, `'\\''`)}'`;
}

export interface InitFields {
  server: string;
  datastore: string;
  namespace: string;
  authId: string;
}

/**
 * The `nestegg init` command for the entered values. Empty required fields
 * show as `<NAME>`; `host:port` becomes `--server host --port N`. Never
 * contains a secret: the token secret is entered in the terminal.
 */
export function initCommand(v: InitFields): string {
  let server = v.server.trim();
  let port: string | null = null;
  const bracket = /^\[(.+)\]:(\d{1,5})$/.exec(server);
  const plain = /^([^:]+):(\d{1,5})$/.exec(server);
  if (bracket) [, server, port] = bracket as unknown as [string, string, string];
  else if (plain) [, server, port] = plain as unknown as [string, string, string];
  const options = [
    `--server ${server ? shellQuote(server) : "<SERVER>"}`,
    ...(port ? [`--port ${port}`] : []),
    `--datastore ${v.datastore.trim() ? shellQuote(v.datastore.trim()) : "<DATASTORE>"}`,
    `--auth-id ${v.authId.trim() ? shellQuote(v.authId.trim()) : "'<USER@REALM!TOKEN>'"}`,
  ];
  if (v.namespace.trim()) options.push(`--namespace ${shellQuote(v.namespace.trim())}`);
  // One option per line (with a trailing backslash) so the command stays readable; it still pastes as one command.
  return ["sudo nestegg init", ...options].join(" \\\n  ");
}
