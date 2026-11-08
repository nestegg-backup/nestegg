// Session token handling (ui/README.md, requirement 3).
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, ApiFailure } from "../src/lib/api";
import { authHeaders, initSession, sessionToken, tokenFromHash, type SessionEnv } from "../src/lib/session";
import { authorized } from "../mock/plugin";

function memoryStorage() {
  const data = new Map<string, string>();
  return {
    data,
    getItem: vi.fn((k: string) => data.get(k) ?? null),
    setItem: vi.fn((k: string, v: string) => void data.set(k, v)),
  };
}

function env(hash: string, storage = memoryStorage()) {
  const replaceState = vi.fn();
  const e: SessionEnv = {
    location: { hash, pathname: "/", search: "" },
    history: { replaceState },
    storage,
  };
  return { e, replaceState, storage };
}

// localStorage must never be touched; any access fails the test.
const localStorageSpy = { getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(), key: vi.fn(), length: 0 };

beforeEach(() => {
  vi.stubGlobal("localStorage", localStorageSpy);
});
afterEach(() => {
  vi.unstubAllGlobals();
  for (const f of [localStorageSpy.getItem, localStorageSpy.setItem, localStorageSpy.removeItem, localStorageSpy.clear]) f.mockClear();
});

describe("session token", () => {
  it("is taken from #token= in the address", () => {
    expect(tokenFromHash("#token=abc123")).toBe("abc123");
    expect(tokenFromHash("#token=a%2Bb")).toBe("a+b");
    expect(tokenFromHash("#/pc/self")).toBeNull();
    expect(tokenFromHash("")).toBeNull();
    const { e } = env("#token=abc123");
    expect(initSession(e)).toBe(true);
    expect(sessionToken()).toBe("abc123");
  });

  it("is removed from the address bar", () => {
    const { e, replaceState } = env("#token=abc123");
    e.location.search = "?scenario=ok";
    initSession(e);
    expect(replaceState).toHaveBeenCalledWith(null, "", "/?scenario=ok");
  });

  it("is stored in sessionStorage and survives a reload of the tab", () => {
    const storage = memoryStorage();
    initSession(env("#token=abc123", storage).e);
    expect(storage.data.get("nestegg.session")).toBe("abc123");
    // Reload: no fragment any more, the token comes from sessionStorage.
    const reload = env("", storage);
    expect(initSession(reload.e)).toBe(false);
    expect(sessionToken()).toBe("abc123");
    expect(reload.replaceState).not.toHaveBeenCalled();
  });

  it("works in memory if the browser blocks storage", () => {
    const { e } = env("#token=xyz", null as never);
    e.storage = null;
    expect(initSession(e)).toBe(true);
    expect(authHeaders()).toEqual({ Authorization: "Bearer xyz" });
  });

  it("never uses localStorage", () => {
    const storage = memoryStorage();
    initSession(env("#token=abc123", storage).e);
    initSession(env("", storage).e);
    expect(localStorageSpy.getItem).not.toHaveBeenCalled();
    expect(localStorageSpy.setItem).not.toHaveBeenCalled();
  });
});

describe("API requests", () => {
  it("send the token as Bearer header, without cookies, to relative URLs", async () => {
    initSession(env("#token=abc123").e);
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ version: "0.1.0", api_version: "1" }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    await api.health();
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("api/v1/health");
    expect((init.headers as Record<string, string>).Authorization).toBe("Bearer abc123");
    expect(init.credentials).toBe("omit");
  });

  it("turn a 401 into 'session ended'", async () => {
    initSession(env("#token=old").e);
    vi.stubGlobal("fetch", vi.fn(async () => new Response("Not authorized.\n", { status: 401 })));
    const err = await api.status().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiFailure);
    expect((err as ApiFailure).kind).toBe("unauthorized");
  });
});

describe("mock auth", () => {
  it("accepts only the exact Bearer token, like nestegg ui", () => {
    expect(authorized("Bearer dev", "dev")).toBe(true);
    expect(authorized("Bearer devx", "dev")).toBe(false);
    expect(authorized("dev", "dev")).toBe(false);
    expect(authorized(undefined, "dev")).toBe(false);
  });
});
