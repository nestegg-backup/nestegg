// Reading the answer of `POST /pbs/check`. An item is passed, failed or not checked (it was skipped,
// for example because the server is asleep). Daemons from before `status` existed know only `ok`.

import type { CheckStatus, PbsCheckItem, PbsCheckResult } from "./types";

/** The daemon's name of the reachability item; the page treats it specially. */
export const REACHABLE_ITEM = "PBS reachable";

export function itemStatus(item: PbsCheckItem): CheckStatus {
  return item.status ?? (item.ok ? "passed" : "failed");
}

export interface CheckSummary {
  failed: number;
  notChecked: number;
  /** The server did not answer and nobody asked to wake it. */
  asleep: boolean;
  /** The server did not answer (and, with a wake attempt, not even after it). */
  unreachable: boolean;
}

/** What is known about the backup server and since when. `reachable` null or absent: nothing known. */
export interface Reach {
  reachable?: boolean | null;
  checked_at?: string | null;
}

/**
 * What a connection check says about the server, or null when it has no such item (for example
 * "no configuration"). A server that did not answer is "not reachable" whether the item failed or
 * was only "not checked" (asleep, nobody asked to wake it): in both cases it did not answer.
 */
export function reachOfCheck(result: PbsCheckResult): Reach | null {
  const item = result.checks.find((c) => c.name === REACHABLE_ITEM);
  if (!item) return null;
  return { reachable: itemStatus(item) === "passed", checked_at: result.checked_at };
}

/**
 * The newest thing the page knows about the server. `status.pbs` holds what the last backup run
 * found out; the daemon does not record the result of `POST /pbs/check` there. The page therefore
 * keeps the result of its own check, and it wins until `/status` reports a later time.
 */
export function newestReach(pbs: Reach, check: Reach | null): Reach {
  const fromStatus: Reach = { reachable: pbs.reachable ?? null, checked_at: pbs.checked_at ?? null };
  if (!check || check.reachable == null || !check.checked_at) return fromStatus;
  if (fromStatus.reachable == null || !fromStatus.checked_at) return check;
  return Date.parse(check.checked_at) >= Date.parse(fromStatus.checked_at) ? check : fromStatus;
}

export function summarizeCheck(result: PbsCheckResult): CheckSummary {
  let failed = 0;
  let notChecked = 0;
  let asleep = false;
  let unreachable = false;
  for (const c of result.checks) {
    const s = itemStatus(c);
    if (s === "failed") failed += 1;
    if (s === "not_checked") notChecked += 1;
    if (c.name === REACHABLE_ITEM) {
      asleep = s === "not_checked";
      unreachable = s === "failed";
    }
  }
  return { failed, notChecked, asleep, unreachable };
}
