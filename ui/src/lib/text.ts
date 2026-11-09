// Sentences built from derived state. Separate from the components so they
// can be tested with both languages.

import type { Messages } from "../i18n";
import { ageOf, errorKind, machineKind, runEnd, selfKind, setupGuide, type MachineKind, type SelfKind, type ServerBackups } from "./derive";
import type { Formatter } from "./format";
import type { Machine, Run, Status } from "./types";

/** The newest run was cancelled and no success came after it. */
function cancelledAfter(last: Run | null, success: Run | null): Run | null {
  return last && last.state === "cancelled" && (!success || last.started_at > success.started_at) ? last : null;
}

export interface Summary {
  kind: SelfKind;
  lead: string;
  text: string;
}

/**
 * Status sentence and explanation for this PC.
 *
 * `server` is what the PBS holds (null while unknown). The daemon derives
 * health and last_success from its local state only, which is empty after a
 * reinstall; the snapshots on the server keep the texts from claiming that
 * there is no backup at all.
 */
export function selfSummary(s: Status, t: Messages, f: Formatter, now: Date, server: ServerBackups | null = null): Summary {
  const kind = selfKind(s);
  const m = (iso: string) => f.moment(iso, now);
  const last = s.last_run ?? null;
  const success = s.last_success ?? null;
  // Times shown to the user are start times, like in the run history and the snapshot names.
  const successAt = success?.started_at ?? server?.latest ?? null;
  const lastSuccessText = successAt ? t.self.lastSuccessSentence(m(successAt)) : t.self.noSuccessYet;
  switch (kind) {
    case "unconfigured":
      return { kind, lead: t.self.unconfiguredLead, text: t.self.unconfiguredText };
    case "running":
      return { kind, lead: t.self.runningLead, text: t.self.runningText };
    case "key_missing":
      // No key at all is a different step than a key whose copy is not confirmed yet.
      if (setupGuide(s)?.current === "key_create") return { kind, lead: t.self.keyCreateLead, text: t.self.keyCreateText };
      return { kind, lead: t.self.keyLead, text: successAt ? `${t.self.keyText} ${lastSuccessText}` : t.self.keyText };
    case "failing": {
      // The contract allows health=failing without run details; then the text stays undated.
      const failed = last && last.state === "failed" ? last : null;
      const ek = failed ? errorKind(failed) : null;
      return {
        kind,
        lead: failed ? t.self.failingLead(m(failed.started_at)) : t.self.failingLeadUndated,
        text: ek ? `${t.explain[ek]} ${lastSuccessText}` : lastSuccessText,
      };
    }
    case "overdue": {
      const age = ageOf(success ? runEnd(success) : (successAt ?? last?.started_at ?? now.toISOString()), now);
      const parts = [lastSuccessText];
      if (last?.state === "pbs_unreachable") parts.push(t.self.overduePbs);
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      // Every promise of an automatic retry depends on the schedule being switched on.
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      else parts.push(last?.state === "pbs_unreachable" ? t.self.autoRetry : t.self.overdueCatchUp);
      return { kind, lead: t.self.overdueLead(age.days, age.hours), text: parts.join(" ") };
    }
    case "incomplete": {
      const parts = [t.self.incompleteText];
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      else if (s.schedule.next_run) {
        parts.push(new Date(s.schedule.next_run) <= now ? t.self.nextRunDue : t.self.nextRun(m(s.schedule.next_run)));
      }
      return {
        kind,
        lead: successAt ? t.self.incompleteLead(m(successAt)) : t.self.incompleteLeadUndated,
        text: parts.join(" "),
      };
    }
    case "never": {
      const parts: string[] = [];
      // No backup from this installation, but older ones on the server (e.g. after a reinstall).
      const earlier = server && server.count > 0 && server.latest ? server : null;
      if (earlier) parts.push(t.self.neverServer(earlier.count, m(earlier.latest!)));
      if (last && last.state === "cancelled") {
        parts.push(t.self.neverCancelled(m(last.started_at)));
      } else if (last && last.state !== "succeeded") {
        parts.push(t.self.neverAttempt(m(last.started_at)));
        const ek = errorKind(last);
        if (ek) parts.push(t.explain[ek]);
      }
      if (s.schedule.enabled && s.schedule.next_run && new Date(s.schedule.next_run) > now)
        parts.push(t.self.neverNext(m(s.schedule.next_run)));
      else parts.push(t.self.neverNow);
      return { kind, lead: earlier ? t.self.neverLeadServer : t.self.neverLead, text: parts.join(" ") };
    }
    default: {
      const parts = [t.self.okText];
      const pbsAfterSuccess =
        !!last && last.state === "pbs_unreachable" && (!success || last.started_at > success.started_at);
      if (pbsAfterSuccess) parts.push(t.self.lastAttemptPbs(m(last.started_at)));
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      else if (pbsAfterSuccess) parts.push(t.self.autoRetry);
      else if (s.schedule.next_run) {
        parts.push(new Date(s.schedule.next_run) <= now ? t.self.nextRunDue : t.self.nextRun(m(s.schedule.next_run)));
      }
      return {
        kind,
        // health=ok without last_success is allowed by the contract: still backed up, just undated.
        lead: successAt ? t.self.okLead(m(successAt)) : t.self.okLeadUndated,
        text: parts.join(" "),
      };
    }
  }
}

/** One short line for the list pane. `server` as in selfSummary. */
export function selfRowText(s: Status, t: Messages, f: Formatter, now: Date, server: ServerBackups | null = null): string {
  const kind = selfKind(s);
  const successAt = s.last_success?.started_at ?? server?.latest ?? null;
  switch (kind) {
    case "unconfigured":
      return t.row.unconfigured;
    case "running":
      return t.row.running;
    case "key_missing":
      return setupGuide(s)?.current === "key_create" ? t.row.keyCreate : t.row.keyMissing;
    case "incomplete":
      return successAt ? t.row.incomplete(f.moment(successAt, now), f.dayMonth(successAt)) : t.row.incompleteUndated;
    case "failing": {
      const failed = s.last_run && s.last_run.state === "failed" ? s.last_run : null;
      return failed ? t.row.failing(f.dayMonth(failed.started_at)) : t.row.failingUndated;
    }
    case "overdue":
      return successAt ? t.row.overdue(f.dayMonth(successAt)) : t.row.overdueUndated;
    case "never":
      return server && server.count > 0 ? t.row.neverServer : t.row.never;
    default:
      return successAt ? t.row.ok(f.moment(successAt, now), f.dayMonth(successAt)) : t.row.okUndated;
  }
}

export interface MachineSummary {
  kind: MachineKind;
  lead: string;
  text: string;
}

export function machineSummary(mc: Machine, t: Messages, f: Formatter, now: Date, fallbackOverdueHours?: number): MachineSummary {
  const kind = machineKind(mc, now, fallbackOverdueHours);
  const m = (iso: string) => f.moment(iso, now);
  const last = mc.last_snapshot ?? null;
  const report = mc.report ?? null;
  const pbsNewer =
    report?.result === "pbs_unreachable" && (!last || (report.run_started_at ?? report.reported_at) > last);
  switch (kind) {
    case "failing": {
      const at = report!.run_started_at ?? report!.reported_at;
      const ek = report!.error?.kind ?? "internal";
      const tail = last ? t.other.lastSuccess(m(last)) : t.self.noSuccessYet;
      return { kind, lead: t.other.failingLead(m(at)), text: `${t.explain[ek]} ${tail}` };
    }
    case "never":
      return { kind, lead: t.other.neverLead, text: t.other.neverText };
    case "overdue": {
      const age = ageOf(last!, now);
      const text = pbsNewer ? `${t.other.overdueText(m(last!))} ${t.other.pbsText}` : t.other.overdueText(m(last!));
      return { kind, lead: t.other.overdueLead(age.days, age.hours), text };
    }
    default:
      return {
        kind,
        lead: t.other.okLead(m(last!)),
        text: pbsNewer ? `${t.other.okText} ${t.other.pbsText}` : t.other.okText,
      };
  }
}

export function machineRowText(mc: Machine, t: Messages, f: Formatter, now: Date, fallbackOverdueHours?: number): string {
  const kind = machineKind(mc, now, fallbackOverdueHours);
  const last = mc.last_snapshot ?? null;
  switch (kind) {
    case "failing":
      return t.row.failing(f.dayMonth(mc.report!.run_started_at ?? mc.report!.reported_at));
    case "overdue":
      return t.row.overdue(f.dayMonth(last!));
    case "key_missing":
      return t.row.keyMissing;
    case "never":
      return t.row.never;
    default:
      return t.row.ok(f.moment(last!, now), f.dayMonth(last!));
  }
}

/** Server line in the list pane: the result of the last check, nothing more. */
export function serverRow(s: Status, t: Messages, f: Formatter, now: Date): { tone: "neutral" | "pbs"; text: string } {
  const p = s.pbs;
  if (p.reachable == null || !p.checked_at) return { tone: "neutral", text: t.row.serverUnknown };
  const mo = f.moment(p.checked_at, now);
  const today = mo.rel === "today";
  if (p.reachable) {
    return { tone: "neutral", text: today ? t.row.serverReachable(mo.time) : t.row.serverReachableDay(f.dayMonth(p.checked_at)) };
  }
  return { tone: "pbs", text: today ? t.row.serverUnreachable(mo.time) : t.row.serverUnreachableDay(f.dayMonth(p.checked_at)) };
}
