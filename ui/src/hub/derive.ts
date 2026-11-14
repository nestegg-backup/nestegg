// State of the computers in the hub, built on the interpretation the local
// interface already uses for the other computers of a household
// (`machineKind`, `machineSummary` and their wording in ../lib). The hub only
// adds what it knows beyond the status report: the PBS's own check of the
// newest snapshot (`last_snapshot_verified`), and the reasons an otherwise
// successful backup is incomplete.

import type { Messages } from "../i18n";
import { machineKind, machineName, type MachineKind, type Tone } from "../lib/derive";
import type { Formatter } from "../lib/format";
import { machineSummary } from "../lib/text";
import type { Machine } from "../lib/types";
import type { HubMachine, HubMachines } from "./types";

/**
 * - `failing`: the newest report says the last backup failed (after the newest snapshot)
 * - `damaged`: the PBS's own check found the newest snapshot damaged
 * - `overdue`: no new snapshot within the computer's "overdue after"
 * - `incomplete`: the newest backup worked but left something out
 * - `first_backup`: joined, the first backup is not on the server yet
 * - `unknown`: the hub has never been able to read the PBS
 * - `ok`
 */
export type HubKind = "failing" | "damaged" | "overdue" | "incomplete" | "first_backup" | "unknown" | "ok";

const ORDER: HubKind[] = ["failing", "damaged", "overdue", "incomplete", "unknown", "first_backup", "ok"];

/** The fallback "overdue after" of the local interface when a report names none. */
const FALLBACK_OVERDUE_HOURS = 36;

/**
 * The hub's computer in the shape the local interface reads. A computer can only join a hub once
 * the household master key copy is proven, and it backs up with that key (decision 0034): its own
 * `key_exported` therefore says nothing about whether its backups can be opened, and is dropped.
 */
export function asMachine(m: HubMachine): Machine {
  const report = m.report ? { ...m.report, key_exported: null } : null;
  return { backup_id: m.backup_id, hostname: m.hostname, os: m.os ?? null, last_snapshot: m.last_snapshot ?? null, report };
}

/**
 * The time the PBS data is about. When the PBS did not answer now, the list is what the hub read
 * last: whether a computer was overdue is judged at that time, not against data that cannot change.
 */
export function asOf(data: Pick<HubMachines, "pbs_reachable" | "pbs_read_at">, now: Date): Date | null {
  if (!data.pbs_read_at) return null;
  return data.pbs_reachable ? now : new Date(data.pbs_read_at);
}

/** Reasons an otherwise successful newest backup left something out (English, from the computer). */
export function incompleteReasons(m: HubMachine): string[] {
  const r = m.report;
  if (!r || r.result !== "succeeded" || !m.last_snapshot) return [];
  return (r.incomplete ?? []).filter((x) => typeof x === "string" && x.trim() !== "");
}

function baseKind(m: HubMachine, at: Date): MachineKind {
  return machineKind(asMachine(m), at, FALLBACK_OVERDUE_HOURS);
}

export function hubKind(m: HubMachine, data: Pick<HubMachines, "pbs_reachable" | "pbs_read_at">, now: Date): HubKind {
  const at = asOf(data, now);
  if (!at) return "unknown";
  const base = baseKind(m, at);
  if (base === "failing") return "failing";
  if (base === "never") return "first_backup";
  if (m.last_snapshot_verified === false) return "damaged";
  if (base === "overdue") return "overdue";
  if (incompleteReasons(m).length) return "incomplete";
  return "ok";
}

export function hubTone(kind: HubKind): Tone {
  switch (kind) {
    case "failing":
    case "damaged":
      return "bad";
    case "overdue":
    case "incomplete":
      return "warn";
    case "ok":
      return "ok";
    default:
      return "neutral";
  }
}

/** Problems first, then by name (as in the sidebar of the local interface). */
export function sortHubMachines(machines: HubMachine[], data: Pick<HubMachines, "pbs_reachable" | "pbs_read_at">, now: Date): HubMachine[] {
  return machines
    .map((m) => ({ m, k: ORDER.indexOf(hubKind(m, data, now)) }))
    .sort((a, b) => a.k - b.k || hubName(a.m).localeCompare(hubName(b.m)))
    .map((x) => x.m);
}

export function hubName(m: HubMachine): string {
  return machineName(asMachine(m));
}

export interface HubSummary {
  kind: HubKind;
  tone: Tone;
  lead: string;
  text: string;
}

/** The status sentence of one computer and its explanation. */
export function hubSummary(m: HubMachine, data: Pick<HubMachines, "pbs_reachable" | "pbs_read_at">, t: Messages, f: Formatter, now: Date): HubSummary {
  const kind = hubKind(m, data, now);
  const tone = hubTone(kind);
  const mo = (iso: string) => f.moment(iso, now);
  const last = m.last_snapshot ?? null;
  const h = t.hub.state;
  switch (kind) {
    case "unknown":
      return { kind, tone, lead: h.unknownLead, text: h.unknownText };
    case "first_backup":
      return { kind, tone, lead: h.firstLead, text: h.firstText };
    case "damaged":
      return { kind, tone, lead: h.damagedLead(mo(last!)), text: h.damagedText };
    case "incomplete":
      return { kind, tone, lead: t.self.incompleteLead(mo(last!)), text: h.incompleteText };
    case "ok":
      // Judged at the time the PBS was read: "overdue" against today would blame the computer for stale data.
      return { kind, tone, lead: t.other.okLead(mo(last!)), text: data.pbs_reachable ? t.other.okText : h.okStale };
    default: {
      // failing and overdue: the local interface's sentences for another computer of the household.
      const s = machineSummary(asMachine(m), t, f, now, FALLBACK_OVERDUE_HOURS);
      return { kind, tone, lead: s.lead, text: s.text };
    }
  }
}

export interface Overview {
  tone: Tone;
  lead: string;
  text: string;
}

/** The sentence on top of the list: how the household as a whole is doing. */
export function overview(data: HubMachines, t: Messages, now: Date): Overview {
  const s = t.hub.summary;
  const n = data.machines.length;
  if (n === 0) return { tone: "neutral", lead: s.emptyLead, text: s.emptyText };
  if (!data.pbs_read_at) return { tone: "neutral", lead: s.unknownLead, text: s.unknownText(n) };
  const kinds = data.machines.map((m) => hubKind(m, data, now));
  const problems = kinds.filter((k) => k !== "ok" && k !== "first_backup");
  const waiting = kinds.filter((k) => k === "first_backup").length;
  if (!problems.length) return { tone: "ok", lead: s.okLead, text: waiting ? s.waitingText(n, waiting) : s.okText(n) };
  const tone: Tone = problems.some((k) => hubTone(k) === "bad") ? "bad" : "warn";
  return { tone, lead: s.problemsLead(problems.length, n), text: s.problemsText };
}

/** Whole minutes left of a code (rounded up), 0 when it has expired. */
export function minutesLeft(expiresAt: string, now: Date): number {
  const ms = new Date(expiresAt).getTime() - now.getTime();
  return ms <= 0 ? 0 : Math.ceil(ms / 60_000);
}

/** "Valid until 14:32, 28 more minutes." */
export function validityText(expiresAt: string, t: Messages, f: Formatter, now: Date): string {
  const left = minutesLeft(expiresAt, now);
  const until = f.time(expiresAt);
  if (left === 0) return t.hub.dialog.expired(until);
  return left === 1 ? t.hub.dialog.validLast(until) : t.hub.dialog.valid(until, left);
}

/** Computers in `after` that were not in `before` (by machine id), e.g. the one that just joined. */
export function newMachines(before: ReadonlySet<string>, after: readonly HubMachine[]): HubMachine[] {
  return after.filter((m) => !before.has(m.machine_id));
}

/** The command for a hub without a package; `<FILE>` (or `<DATEI>`) cannot be copied, see Cmd. */
export function packageCommand(t: Messages): string {
  return `sudo nestegg hub package add ${t.hub.dialog.packagePlaceholder}`;
}

export const MASTER_KEY_CONFIRM_COMMAND = "sudo nestegg hub master-key confirm";
