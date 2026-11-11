// Sentences built from derived state. Separate from the components so they
// can be tested with both languages.

import type { Messages } from "../i18n";
import { ageOf, errorKind, hasPbs, isUnknownRun, knownErrorKind, machineKind, otherWarnings, runEnd, runState, runTarget, runTrigger, selfKind, setupGuide, stickOnly, stickTone, stickView, succeededKind, type MachineKind, type SelfKind, type ServerBackups, type StickKind, type StickView } from "./derive";
import type { Reach } from "./check";
import type { Formatter } from "./format";
import type { Machine, Run, Status, TargetStatus } from "./types";

/** The newest run was cancelled and no success came after it. */
function cancelledAfter(last: Run | null, success: Run | null): Run | null {
  return last && last.state === "cancelled" && (!success || last.started_at > success.started_at) ? last : null;
}

/** As in the daemon (`FUTURE_TOLERANCE_MINUTES`): a stamp further ahead than this says nothing about the age. */
const CLOCK_TOLERANCE_MS = 5 * 60_000;

/**
 * The plain-language explanation of a run's error. A failed check after writing means something
 * else on the stick (no server to blame) than on the backup server.
 */
export function explainOf(run: Run, t: Messages): string | null {
  const k = errorKind(run);
  if (!k) return null;
  if (k === "verification_failed" && runTarget(run) === "stick") return t.stick.verifyText;
  return t.explain[k];
}

export interface Summary {
  kind: SelfKind;
  lead: string;
  text: string;
  /** Automatic backups are switched off although nothing else is wrong: the page offers the way to the settings. */
  autoOff?: boolean;
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
  // Stick only and the stick is not plugged in: nothing starts by itself until it is.
  const waitStick = stickOnly(s) && s.schedule.enabled && s.targets?.stick != null && stickView(s.targets.stick).plugged === false;
  // The token secret comes before the key and before the first backup (see setupGuide).
  if ((kind === "key_missing" || kind === "never") && setupGuide(s)?.current === "credentials") {
    return { kind, lead: t.self.credentialsLead, text: t.self.credentialsText };
  }
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
      // The last entry was written by a newer version: say that, not "failed" and not "cancelled".
      // Switched off: nothing is retried by itself, so the text says that too and links to the settings.
      const off = s.schedule.enabled ? {} : { autoOff: true };
      const tail = s.schedule.enabled ? "" : ` ${t.self.scheduleOff}`;
      if (last && isUnknownRun(last)) {
        return {
          kind,
          lead: t.self.unknownLead(m(last.started_at)),
          text: `${t.self.unknownText} ${lastSuccessText}${tail}`,
          ...off,
        };
      }
      // The contract allows health=failing without run details; then the text stays undated.
      const failed = last && last.state === "failed" ? last : null;
      const explained = failed ? explainOf(failed, t) : null;
      return {
        kind,
        lead: failed ? t.self.failingLead(m(failed.started_at)) : t.self.failingLeadUndated,
        text: (explained ? `${explained} ${lastSuccessText}` : lastSuccessText) + tail,
        ...off,
      };
    }
    case "overdue": {
      // The time of the last backup lies in the future: the clock was wrong when it was written. Counting
      // hours from it ("not backed up for 0 hours") would be nonsense; say what is known instead.
      if (successAt && new Date(successAt).getTime() - now.getTime() > CLOCK_TOLERANCE_MS) {
        const parts = [t.self.clockText(m(successAt))];
        if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
        return { kind, lead: t.self.clockLead, text: parts.join(" "), ...(s.schedule.enabled ? {} : { autoOff: true }) };
      }
      const age = ageOf(success ? runEnd(success) : (successAt ?? last?.started_at ?? now.toISOString()), now);
      const parts = [lastSuccessText];
      if (last?.state === "pbs_unreachable") parts.push(t.self.overduePbs);
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      // Every promise of an automatic retry depends on the schedule being switched on.
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      else if (waitStick) parts.push(t.stick.waitPlug);
      else parts.push(last?.state === "pbs_unreachable" ? t.self.autoRetry : t.self.overdueCatchUp);
      return { kind, lead: t.self.overdueLead(age.days, age.hours), text: parts.join(" ") };
    }
    case "incomplete": {
      // The daemon may report health=incomplete without saying what is missing: do not point at an empty list.
      const parts = [success?.incomplete?.length ? t.self.incompleteText : t.self.incompleteTextNoList];
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      else if (waitStick) parts.push(t.stick.waitPlug);
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
      } else if (last && isUnknownRun(last)) {
        parts.push(t.self.neverUnknown(m(last.started_at)));
      } else if (last && last.state !== "succeeded") {
        parts.push(t.self.neverAttempt(m(last.started_at)));
        const explained = explainOf(last, t);
        if (explained) parts.push(explained);
      }
      if (waitStick) parts.push(t.stick.waitPlug);
      else if (s.schedule.enabled && s.schedule.next_run && new Date(s.schedule.next_run) > now)
        parts.push(t.self.neverNext(m(s.schedule.next_run)));
      else parts.push(t.self.neverNow);
      // Switched off: without this the page would let the reader wait for a first backup that never starts.
      if (!s.schedule.enabled) parts.push(t.self.scheduleOff);
      return {
        kind,
        lead: earlier ? t.self.neverLeadServer : t.self.neverLead,
        text: parts.join(" "),
        ...(s.schedule.enabled ? {} : { autoOff: true }),
      };
    }
    default: {
      // The lead must not say "everything is fine" above a notice about damaged backups.
      const damaged = (s.server_verify_failed?.length ?? 0) > 0 || (server?.damaged ?? 0) > 0;
      // Switched off: "Everything is fine" would hide that nothing happens by itself any more. Say it
      // first, with the time of the last backup and the way to the settings.
      if (!s.schedule.enabled) {
        const off: string[] = [];
        if (successAt) off.push(lastSuccessText);
        const pbsLate = !!last && last.state === "pbs_unreachable" && (!success || last.started_at > success.started_at);
        if (pbsLate) off.push(t.self.lastAttemptPbs(m(last.started_at)));
        const wasCancelled = cancelledAfter(last, success);
        if (wasCancelled) off.push(t.self.lastCancelled(m(wasCancelled.started_at)));
        if (damaged) off.push(t.self.okOffDamaged);
        off.push(t.self.okOffText);
        return { kind, lead: t.self.okOffLead, text: off.join(" "), autoOff: true };
      }
      // The server is fine, the stick is not: "Everything is fine" would be untrue for the computer.
      const stickTs = hasPbs(s) && s.targets?.pbs ? s.targets?.stick : undefined;
      const stickBad = !!stickTs && ["bad", "warn"].includes(stickTone(stickView(stickTs).kind));
      const parts = [damaged ? t.self.okTextDamaged : stickBad ? t.stick.okServerOnly : t.self.okText];
      const pbsAfterSuccess =
        !!last && last.state === "pbs_unreachable" && (!success || last.started_at > success.started_at);
      if (pbsAfterSuccess) parts.push(t.self.lastAttemptPbs(m(last.started_at)));
      const cancelled = cancelledAfter(last, success);
      if (cancelled) parts.push(t.self.lastCancelled(m(cancelled.started_at)));
      if (pbsAfterSuccess) parts.push(t.self.autoRetry);
      else if (waitStick) parts.push(t.stick.waitPlug);
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

/**
 * The "Result" cell of a history row. A state or trigger this version does not know has its own
 * text (`runState`/`runTrigger` turn any unrecognised word into "unknown"): it can never end up as
 * "cancelled" or "backed up". `count`: attempts folded into the row (server not reachable).
 */
export function runResultText(r: Run, count: number, damaged: ReadonlySet<string>, t: Messages): string {
  switch (runState(r)) {
    case "succeeded":
      switch (succeededKind(r, damaged)) {
        case "damaged":
          return t.runs.damaged;
        case "incomplete":
          return t.runs.succeededIncomplete;
        case "warnings":
          return t.runs.succeededWarnings(otherWarnings(r).length);
        case "ok":
          switch (runTrigger(r)) {
            case "timer":
              return t.runs.succeeded;
            case "manual":
            case "cli":
              return t.runs.succeededManual;
            case "plug":
              return t.runs.succeededPlug;
            case "unknown":
              return t.runs.succeededOtherStart;
          }
      }
      return t.runs.succeeded;
    case "failed":
      return t.runs.failed(t.reason[errorKind(r) ?? "internal"]);
    case "pbs_unreachable":
      return count > 1 ? t.runs.pbsTimes(count) : t.runs.pbs;
    case "stick_not_plugged_in":
      return count > 1 ? t.runs.stickAbsentTimes(count) : t.runs.stickAbsent;
    case "cancelled":
      return t.runs.cancelled;
    case "unknown":
      return t.runs.unknown;
    default:
      // A run that is still going is not part of the history; if one shows up, it has no result yet.
      return t.runs.unknown;
  }
}

/**
 * With a server and a stick, the sidebar line is about the server, unless the server is fine and
 * the stick is not: then it names the stick's problem (or that it is backing up right now).
 */
export function stickRowOverride(s: Status, t: Messages, f: Formatter): string | null {
  const ts = s.targets?.stick;
  if (!ts || !hasPbs(s) || !s.targets?.pbs) return null;
  const own = selfKind(s);
  if (own !== "ok" && own !== "incomplete") return null;
  const v = stickView(ts);
  const success = ts.last_success?.started_at ?? null;
  switch (v.kind) {
    case "running":
      return t.row.stickRunning;
    case "full":
      return t.row.stickFull;
    case "store_damaged":
      return t.row.stickDamaged;
    case "damaged":
    case "verify_failed":
    case "key_mismatch":
    case "unsupported":
    case "failing":
    case "unknown":
      return t.row.stickFailing;
    case "overdue":
      return success ? t.row.stickOverdue(f.dayMonth(success)) : t.row.overdueUndated;
    default:
      return null;
  }
}

/** One short line for the list pane. `server` as in selfSummary. */
export function selfRowText(s: Status, t: Messages, f: Formatter, now: Date, server: ServerBackups | null = null, withStick = true): string {
  const stick = withStick ? stickRowOverride(s, t, f) : null;
  if (stick) return stick;
  const kind = selfKind(s);
  const successAt = s.last_success?.started_at ?? server?.latest ?? null;
  if ((kind === "key_missing" || kind === "never") && setupGuide(s)?.current === "credentials") return t.row.credentials;
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
      if (isUnknownRun(s.last_run)) return t.row.unknownRun;
      const failed = s.last_run && s.last_run.state === "failed" ? s.last_run : null;
      return failed ? t.row.failing(f.dayMonth(failed.started_at)) : t.row.failingUndated;
    }
    case "overdue":
      return successAt ? t.row.overdue(f.dayMonth(successAt)) : t.row.overdueUndated;
    case "never":
      return server && server.count > 0 ? t.row.neverServer : t.row.never;
    default:
      if (!s.schedule.enabled) return successAt ? t.row.okOff(f.moment(successAt, now), f.dayMonth(successAt)) : t.row.okOffUndated;
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
      const ek = knownErrorKind(report!.error?.kind ?? "internal");
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

/** Server line in the list pane: the newest known result of a check (see `newestReach`), nothing more. */
export function serverRow(p: Reach, t: Messages, f: Formatter, now: Date): { tone: "neutral" | "pbs"; text: string } {
  if (p.reachable == null || !p.checked_at) return { tone: "neutral", text: t.row.serverUnknown };
  const mo = f.moment(p.checked_at, now);
  const today = mo.rel === "today";
  if (p.reachable) {
    return { tone: "neutral", text: today ? t.row.serverReachable(mo.time) : t.row.serverReachableDay(f.dayMonth(p.checked_at)) };
  }
  return { tone: "pbs", text: today ? t.row.serverUnreachable(mo.time) : t.row.serverUnreachableDay(f.dayMonth(p.checked_at)) };
}

// ---------------------------------------------------------------- the stick

export interface StickLine {
  view: StickView;
  /** The state, as one sentence. */
  lead: string;
  /** Why, for problems (plain language), or null. */
  why: string | null;
  /** Plugged in, not plugged in, safe to unplug; null while the state does not say. */
  plug: string | null;
}

const STICK_LEAD: Record<Exclude<StickKind, "ok" | "incomplete" | "overdue" | "failing">, (t: Messages) => string> = {
  running: (t) => t.stick.running,
  full: (t) => t.stick.full,
  damaged: (t) => t.stick.damaged,
  verify_failed: (t) => t.stick.verifyFailed,
  store_damaged: (t) => t.stick.storeDamaged,
  key_mismatch: (t) => t.stick.keyMismatch,
  unsupported: (t) => t.stick.unsupported,
  unknown: (t) => t.stick.unknown,
  never: (t) => t.stick.never,
};

/**
 * What the stick row says about the stick target. `inUse`: this page is restoring files from the
 * stick; the status does not know that, so it must not say the stick can be unplugged.
 */
export function stickLine(ts: TargetStatus, t: Messages, f: Formatter, now: Date, inUse = false): StickLine {
  const seen = stickView(ts);
  const view = inUse ? { ...seen, safeToUnplug: false } : seen;
  const m = (iso: string) => f.moment(iso, now);
  const last = ts.last_run ?? null;
  const success = ts.last_success ?? null;
  let lead: string;
  switch (view.kind) {
    case "ok":
      lead = success ? t.stick.ok(m(success.started_at)) : t.stick.okUndated;
      break;
    case "incomplete":
      lead = success ? t.stick.incomplete(m(success.started_at)) : t.stick.okUndated;
      break;
    case "overdue": {
      const age = ageOf(success ? runEnd(success) : now.toISOString(), now);
      lead = t.stick.overdue(age.days, age.hours);
      break;
    }
    case "failing":
      lead = last?.state === "failed" ? t.stick.failing(m(last.started_at)) : t.stick.failingUndated;
      break;
    default:
      lead = STICK_LEAD[view.kind](t);
  }
  const failed = last?.state === "failed" ? last : null;
  const why =
    view.kind === "running" || view.kind === "ok" || view.kind === "incomplete" || view.kind === "never" || view.kind === "overdue"
      ? null
      : view.kind === "unknown"
        ? null
        : failed
          ? explainOf(failed, t)
          : view.kind === "full"
            ? t.explain.stick_full
            : null;
  const plug =
    view.kind === "running" || (inUse && view.plugged !== false)
      ? t.stick.keepIn
      : view.plugged === false
        ? view.kind === "overdue" || view.kind === "never"
          ? `${t.stick.absent} ${t.stick.plugToBackUp}`
          : t.stick.absent
        : view.safeToUnplug
          ? t.stick.safe
          : view.plugged
            ? t.stick.plugged
            : null;
  return { view, lead, why, plug };
}
