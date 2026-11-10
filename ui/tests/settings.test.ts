// Settings validation must match the daemon (crates/nestegg/src/settings.rs).
import { afterEach, describe, expect, it, vi } from "vitest";
import { api, ApiFailure } from "../src/lib/api";
import { DEFAULT_LIMITS, excludeProblem, excludesEverything, overdueCap, settingsProblem } from "../src/lib/settings";
import type { SettingsLimits } from "../src/lib/types";

const current = { interval_hours: 24, overdue_after_hours: 36, wake_on_lan_configured: true };

describe("settings validation", () => {
  it("finds patterns that exclude everything", () => {
    for (const p of ["*", "**", "/", "//", "/*", "/**", "**/*", "*/*", "**/", "?*"]) expect(excludesEverything(p), p).toBe(true);
    for (const p of ["**/.cache", "/home/*/Videos", "*.iso", "!**/keep", "?", "[a-z]*"]) expect(excludesEverything(p), p).toBe(false);
  });

  it("checks single patterns", () => {
    expect(excludeProblem("**/.cache")).toBeNull();
    for (const p of ["", "!", " x", "x ", "-x", "a\nb", "a\u0000b", "a\tb", "a".repeat(257), "**"]) expect(excludeProblem(p), JSON.stringify(p)).not.toBeNull();
    expect(excludeProblem("a".repeat(256))).toBeNull();
  });

  it("counts the length in bytes, like the daemon", () => {
    // 128 two-byte characters are 256 bytes: allowed; one more is 258 bytes.
    expect(excludeProblem("ä".repeat(128))).toBeNull();
    expect(excludeProblem("ä".repeat(129))).toBe("exclude patterns may have at most 256 bytes");
    expect(excludeProblem("a", { ...DEFAULT_LIMITS, excludes: { max_items: 200, max_length: 10 } })).toBeNull();
    expect(excludeProblem("€".repeat(4), { ...DEFAULT_LIMITS, excludes: { max_items: 200, max_length: 10 } })).toContain("at most 10 bytes");
  });

  it("refuses invisible format characters like control characters", () => {
    for (const c of ["\u200b", "\u200e", "\u202e", "\u2066", "\u2060", "\ufeff", "\u061c", "\u0085"])
      expect(excludeProblem(`a${c}b`), JSON.stringify(c)).toContain("control character or an invisible format character");
    expect(excludeProblem("a\u00e4b")).toBeNull();
  });

  it("checks ranges and the overdue rule", () => {
    expect(settingsProblem({ interval_hours: 12 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: 169, overdue_after_hours: 744 }, current)).toContain("between 1 and 168");
    expect(DEFAULT_LIMITS.overdue_after_hours.max).toBe(168);
    expect(settingsProblem({ overdue_after_hours: 168 }, current)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 169 }, current)).toBe(
      "overdue_after_hours must be between 2 and 168 hours through the web interface. A longer time can only be set as root in /etc/nestegg/config.toml.",
    );
    expect(settingsProblem({ interval_hours: 36 }, current)).toContain("greater than interval_hours");
    expect(settingsProblem({ interval_hours: 48, overdue_after_hours: 60 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: null }, current)).toBeNull();
    expect(settingsProblem({}, current)).toBeNull();
  });

  it("computes the cap from the rhythm in effect after the same request", () => {
    // max(168, interval + 72): a weekly rhythm may be overdue after up to 240 hours.
    expect(overdueCap(24)).toBe(168);
    expect(overdueCap(96)).toBe(168);
    expect(overdueCap(97)).toBe(169);
    expect(overdueCap(168)).toBe(240);
    expect(settingsProblem({ interval_hours: 168, overdue_after_hours: 240 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: 168, overdue_after_hours: 241 }, current)).toContain("between 2 and 240 hours");
    // Without the rhythm in the request the rhythm in the file counts.
    expect(settingsProblem({ overdue_after_hours: 240 }, { ...current, interval_hours: 168 })).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 241 }, { ...current, interval_hours: 168 })).toContain("between 2 and 240 hours");
    // A longer rhythm in the same request lifts the cap; a shorter one in the same request lowers it again.
    expect(settingsProblem({ interval_hours: 144, overdue_after_hours: 200 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: 24, overdue_after_hours: 200 }, { ...current, interval_hours: 144 })).toContain("between 2 and 168 hours");
  });

  it("believes a daemon that does not send the numbers on max", () => {
    const old: SettingsLimits = { ...DEFAULT_LIMITS, overdue_after_hours: { min: 2, max: 168, greater_than_interval: true } };
    expect(overdueCap(168, old)).toBe(168);
    expect(settingsProblem({ interval_hours: 168, overdue_after_hours: 200 }, current, old)).toContain("between 2 and 168 hours");
  });

  it("lets a value from the file, above the cap, only be lowered", () => {
    const fromFile = { ...current, overdue_after_hours: 744 };
    const limits: SettingsLimits = {
      ...DEFAULT_LIMITS,
      overdue_after_hours: { ...DEFAULT_LIMITS.overdue_after_hours, max: 744, file_minimum: 744 },
    };
    expect(overdueCap(24, limits)).toBe(744);
    expect(settingsProblem({ overdue_after_hours: 744 }, fromFile, limits)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 200 }, fromFile, limits)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 745 }, fromFile, limits)).toBe(
      "overdue_after_hours must be between 2 and 744 hours through the web interface; the current value of 744 can only be lowered here. A longer time can only be set as root in /etc/nestegg/config.toml.",
    );
    // Other changes are not held up by the long value.
    expect(settingsProblem({ interval_hours: 12 }, fromFile, limits)).toBeNull();
  });

  it("does not judge a pattern by its spelling: the daemon decides which folders it would leave out", () => {
    // Refused by the old spelling rule, accepted by the daemon for a default installation.
    for (const p of ["/Downloads", "/home", "/var", "home", "/usr/local/*"]) expect(settingsProblem({ excludes: [p] }, current), p).toBeNull();
    // What the browser can know is still checked.
    expect(settingsProblem({ excludes: ["a", "a"] }, current)).toContain("listed twice");
    expect(settingsProblem({ excludes: ["-a"] }, current)).toContain("must not start with `-`");
  });

  it("asks the daemon's rule only for patterns that are new, and in the daemon's order", () => {
    const asked: string[] = [];
    const decide = (p: string) => {
      asked.push(p);
      return p === "/alice" ? "exclude pattern `/alice` would leave out the whole folder /home/alice from the backup; an administrator can still set it as root in /etc/nestegg/config.toml." : null;
    };
    const withList = { ...current, excludes: ["/alice", "**/.cache"] };
    expect(settingsProblem({ excludes: ["/alice", "**/.cache", "*.iso"] }, withList, DEFAULT_LIMITS, decide)).toBeNull();
    expect(asked).toEqual(["*.iso"]);
    expect(settingsProblem({ excludes: ["/alice"] }, current, DEFAULT_LIMITS, decide)).toContain("whole folder /home/alice");
    // The list is validated before the folder rule is asked.
    asked.length = 0;
    expect(settingsProblem({ excludes: ["/alice", "/alice"] }, current, DEFAULT_LIMITS, decide)).toContain("listed twice");
    expect(asked).toEqual([]);
  });

  it("refuses terminal-only and unknown fields, and Wake-on-LAN without a MAC", () => {
    expect(settingsProblem({ datastore: "x" }, current)).toContain("cannot be changed here");
    expect(settingsProblem({ wake: true }, current)).toContain("cannot be changed here");
    expect(settingsProblem({ intervall: 1 }, current)).toContain("unknown setting");
    expect(settingsProblem({ wake_on_lan: true }, { ...current, wake_on_lan_configured: false })).toContain("[wake]");
    expect(settingsProblem({ wake_on_lan: false }, { ...current, wake_on_lan_configured: false })).toBeNull();
    expect(settingsProblem({ excludes: Array.from({ length: 201 }, (_, i) => `p${i}`) }, current)).toContain("at most 200");
  });
});

describe("api.updateConfig", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends a PATCH with a JSON body and turns 400 into bad_request", async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      calls.push({ url, init });
      return new Response(JSON.stringify({ code: "bad_request", message: "interval_hours must be between 1 and 168 (one week)" }), { status: 400 });
    });
    const failure = await api.updateConfig({ interval_hours: 500 }).catch((e: unknown) => e);
    expect(failure).toBeInstanceOf(ApiFailure);
    expect((failure as ApiFailure).kind).toBe("bad_request");
    expect((failure as ApiFailure).message).toContain("between 1 and 168");
    expect(calls[0]!.url).toBe("api/v1/config");
    expect(calls[0]!.init.method).toBe("PATCH");
    expect(calls[0]!.init.body).toBe('{"interval_hours":500}');
    expect((calls[0]!.init.headers as Record<string, string>)["Content-Type"]).toBe("application/json");
  });
});
