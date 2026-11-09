// Pure state derivation: what the UI shows for a given API state.
// No strings here except stable ids; texts live in ../i18n.

import { daysBetween, shiftDayKey, type Formatter } from "./format";
import type { Machine, Run, RunErrorKind, Status } from "./types";

export const ACTIVE_STATES = ["starting", "waking_pbs", "inventory", "uploading", "finishing"] as const;

export function isActive(run: Pick<Run, "state"> | null | undefined): boolean {
  return !!run && (ACTIVE_STATES as readonly string[]).includes(run.state);
}

/** What the detail page of this PC is about, in priority order. */
export type SelfKind = "unconfigured" | "running" | "key_missing" | "failing" | "overdue" | "never" | "ok";

export function selfKind(s: Status): SelfKind {
  if (s.health === "unconfigured") return "unconfigured";
  if (s.health === "running" || isActive(s.current_run)) return "running";
  if (s.encryption.enabled && s.encryption.key_exported === false) return "key_missing";
  switch (s.health) {
    case "failing":
      return "failing";
    case "overdue":
      return "overdue";
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

export type DayKind = "ok" | "fail" | "pbs" | "none" | "run";

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
  const rank: DayKind[] = ["ok", "run", "fail", "pbs"];
  const cells: DayCell[] = [];
  for (let i = count - 1; i >= 0; i--) {
    const key = shiftDayKey(todayKey, -i);
    const kinds = byDay.get(key);
    const kind = rank.find((r) => kinds?.has(r)) ?? "none";
    cells.push({ key, kind, today: i === 0 });
  }
  return cells;
}

export function runEvents(runs: Run[], current?: Run | null): { at: string; kind: Exclude<DayKind, "none"> }[] {
  const out: { at: string; kind: Exclude<DayKind, "none"> }[] = [];
  for (const r of runs) {
    if (r.state === "succeeded") out.push({ at: r.started_at, kind: "ok" });
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
export function snapshotEvents(snapshots: { time: string }[]): { at: string; kind: "ok" }[] {
  return snapshots.map((s) => ({ at: s.time, kind: "ok" }));
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
      run.state === "failed" || run.state === "pbs_unreachable" || (run.warnings?.length ?? 0) > 0;
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

/** "every 24 h" / "every 24 h (checked hourly)" -> 24 */
export function intervalHours(schedule: string | undefined | null): number | null {
  const m = /every\s+(\d+)\s*h/i.exec(schedule ?? "");
  return m ? Number(m[1]) : null;
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
