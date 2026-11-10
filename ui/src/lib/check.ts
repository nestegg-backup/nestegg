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
