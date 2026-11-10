// Settings validation must match the daemon (crates/nestegg/src/settings.rs).
import { afterEach, describe, expect, it, vi } from "vitest";
import { api, ApiFailure } from "../src/lib/api";
import { DEFAULT_LIMITS, excludeProblem, excludesEverything, settingsProblem, wholeSourceProblem } from "../src/lib/settings";

const current = { interval_hours: 24, overdue_after_hours: 36, wake_on_lan_configured: true };

describe("settings validation", () => {
  it("finds patterns that exclude everything", () => {
    for (const p of ["*", "**", "/", "//", "/*", "/**", "**/*", "*/*", "**/", "?*"]) expect(excludesEverything(p), p).toBe(true);
    for (const p of ["**/.cache", "/home/*/Videos", "*.iso", "!**/keep", "?", "[a-z]*"]) expect(excludesEverything(p), p).toBe(false);
  });

  it("checks single patterns", () => {
    expect(excludeProblem("**/.cache")).toBeNull();
    for (const p of ["", "!", " x", "x ", "a\nb", "a\u0000b", "a\tb", "a".repeat(257), "**"]) expect(excludeProblem(p), JSON.stringify(p)).not.toBeNull();
    expect(excludeProblem("a".repeat(256))).toBeNull();
  });

  it("checks ranges and the overdue rule", () => {
    expect(settingsProblem({ interval_hours: 12 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: 169, overdue_after_hours: 744 }, current)).toContain("between 1 and 168");
    expect(DEFAULT_LIMITS.overdue_after_hours.max).toBe(168);
    expect(settingsProblem({ overdue_after_hours: 168 }, current)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 169 }, current)).toBe(
      "overdue_after_hours must be between 2 and 168 hours (7 days) through the web interface. A longer time can only be set as root in /etc/nestegg/config.toml.",
    );
    expect(settingsProblem({ interval_hours: 36 }, current)).toContain("greater than interval_hours");
    expect(settingsProblem({ interval_hours: 48, overdue_after_hours: 60 }, current)).toBeNull();
    expect(settingsProblem({ interval_hours: null }, current)).toBeNull();
    expect(settingsProblem({}, current)).toBeNull();
  });

  it("lets a value from the file, above the limit, only be lowered", () => {
    const fromFile = { ...current, overdue_after_hours: 744 };
    const limits = { ...DEFAULT_LIMITS, overdue_after_hours: { ...DEFAULT_LIMITS.overdue_after_hours, max: 744 } };
    expect(settingsProblem({ overdue_after_hours: 744 }, fromFile, limits)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 200 }, fromFile, limits)).toBeNull();
    expect(settingsProblem({ overdue_after_hours: 745 }, fromFile, limits)).toBe(
      "overdue_after_hours must be between 2 and 168 hours (7 days) through the web interface; the current value of 744 can only be lowered here. A longer time can only be set as root in /etc/nestegg/config.toml.",
    );
    // Other changes are not held up by the long value.
    expect(settingsProblem({ interval_hours: 12 }, fromFile, limits)).toBeNull();
  });

  it("refuses an exclude that would drop a whole folder, like the daemon", () => {
    const sources = ["/home", "/etc", "/root", "/usr/local"];
    for (const bad of ["/home", "/home/", "/home/*", "/home/**", "/etc", "/root", "/usr", "/var", "/alice", "/usr/local", "/usr/local/*", "/usr/local/**", "home", "etc", "local"])
      expect(wholeSourceProblem(bad, sources), bad).not.toBeNull();
    for (const ok of ["**/.cache", "**/node_modules", "**/lost+found", "/alice/Downloads", "/home/*/Videos", "*.iso", "node_modules", "/usr/lib", "!/home"])
      expect(wholeSourceProblem(ok, sources), ok).toBeNull();
    expect(wholeSourceProblem("/home/", sources)).toBe(
      "exclude pattern `/home/` would drop the whole top-level directory `/home` from every archive that contains it; an administrator can still set it as root in /etc/nestegg/config.toml.",
    );
    expect(wholeSourceProblem("/usr/local/*", sources)).toBe(
      "exclude pattern `/usr/local/*` would drop the whole backup source `/usr/local`; an administrator can still set it as root in /etc/nestegg/config.toml.",
    );
    expect(wholeSourceProblem("local", sources)).toBe(
      "exclude pattern `local` matches the whole backup source `local` in every archive, which would leave it out of the backup; an administrator can still set it as root in /etc/nestegg/config.toml.",
    );
  });

  it("checks a new exclude against the sources, but keeps one that is already in the list", () => {
    const withSources = { ...current, sources: ["/home", "/etc"], excludes: ["/etc/*"] };
    expect(settingsProblem({ excludes: ["/etc/*", "**/.cache"] }, withSources)).toBeNull();
    expect(settingsProblem({ excludes: ["/etc/*", "/home"] }, withSources)).toContain("top-level directory");
    expect(settingsProblem({ excludes: ["/home/*"] }, { ...current, sources: ["/home"], excludes: [] })).toContain("whole");
    // Without knowing the sources only the top-level rule applies.
    expect(settingsProblem({ excludes: ["/var"] }, current)).toContain("top-level directory `/var`");
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
