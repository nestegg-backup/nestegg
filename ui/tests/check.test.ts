import { describe, expect, it } from "vitest";
import { itemStatus, summarizeCheck } from "../src/lib/check";
import type { PbsCheckResult } from "../src/lib/types";

const at = "2026-11-10T08:00:00Z";

describe("reading the connection check", () => {
  it("an item without status (older daemon) is passed or failed by ok", () => {
    expect(itemStatus({ name: "x", ok: true, detail: "" })).toBe("passed");
    expect(itemStatus({ name: "x", ok: false, detail: "" })).toBe("failed");
  });

  it("status wins over ok: a check that was not made is not a pass and not a failure", () => {
    expect(itemStatus({ name: "x", ok: false, status: "not_checked", detail: "" })).toBe("not_checked");
  });

  it("a sleeping server: asleep, items not checked, nothing failed", () => {
    const r: PbsCheckResult = {
      checked_at: at,
      checks: [
        { name: "configuration", ok: true, status: "passed", detail: "" },
        { name: "PBS reachable", ok: false, status: "not_checked", detail: "no answer" },
        { name: "PBS login and datastore access", ok: false, status: "not_checked", detail: "" },
      ],
    };
    expect(summarizeCheck(r)).toEqual({ failed: 0, notChecked: 2, asleep: true, unreachable: false });
  });

  it("a server that still does not answer after a wake attempt is unreachable", () => {
    const r: PbsCheckResult = {
      checked_at: at,
      checks: [
        { name: "PBS reachable", ok: false, status: "failed", detail: "" },
        { name: "PBS login and datastore access", ok: false, status: "not_checked", detail: "" },
      ],
    };
    expect(summarizeCheck(r)).toEqual({ failed: 1, notChecked: 1, asleep: false, unreachable: true });
  });

  it("an answer of an older daemon (only ok) has no not-checked items", () => {
    const r: PbsCheckResult = { checked_at: at, checks: [{ name: "PBS reachable", ok: false, detail: "no answer" }] };
    expect(summarizeCheck(r)).toEqual({ failed: 1, notChecked: 0, asleep: false, unreachable: true });
  });
});
