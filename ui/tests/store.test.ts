// The store against the answers of the mock, which follow the real daemon (crates/nestegg/src/daemon.rs):
// cancelling what cannot be cancelled is a 404, and an unreachable backup server is a normal 200 answer.
import { afterEach, describe, expect, it, vi } from "vitest";
import { World } from "../mock/world";
import { app } from "../src/lib/store.svelte";

function serve(w: World) {
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    const u = new URL(url, "http://localhost/");
    const path = u.pathname.replace(/^\/api\/v1/, "");
    const body = init?.body ? JSON.parse(String(init.body)) : null;
    const r = w.handle(init?.method ?? "GET", path, u.searchParams, body);
    return new Response(typeof r.body === "string" ? r.body : JSON.stringify(r.body), { status: r.status });
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  app.status = null;
  app.cancel = { state: "idle" };
  app.pbsCheck = { state: "idle" };
});

describe("cancel", () => {
  it("a run that cannot be cancelled (404 from the daemon) is 'gone', with the one honest text", async () => {
    const w = new World("ok", "full");
    serve(w);
    await app.startRun();
    expect(app.status?.current_run).toBeTruthy();
    // Just started: the daemon has no process to signal yet and answers 404.
    await app.cancelRun();
    expect(app.cancel).toMatchObject({ state: "error", kind: "gone" });
  });

  it("the contract's 409 cannot_cancel means the same for the user", async () => {
    vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
      const path = new URL(url, "http://localhost/").pathname.replace(/^\/api\/v1/, "");
      if (path === "/status" || init?.method !== "POST") {
        const w = new World("running", "full");
        return new Response(JSON.stringify(w.handle("GET", path, new URLSearchParams(), null).body), { status: 200 });
      }
      return new Response(JSON.stringify({ code: "cannot_cancel", message: "already finishing" }), { status: 409 });
    });
    await app.poll();
    await app.cancelRun();
    expect(app.cancel).toMatchObject({ state: "error", kind: "gone" });
  });
});

describe("connection check", () => {
  it("a server that is off is a result with a failed item, not an error", async () => {
    const w = new World("overdue", "full");
    w.faults.add("no_wake");
    serve(w);
    await app.poll();
    await app.checkPbs();
    expect(app.pbsCheck.state).toBe("done");
    if (app.pbsCheck.state === "done") {
      expect(app.pbsCheck.result.checks.find((c) => c.name === "PBS reachable")).toMatchObject({ ok: false });
    }
  });

  it("an answer that is not JSON-ok (daemon error) is an error with the daemon's message", async () => {
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ code: "internal", message: "boom" }), { status: 500 }));
    await app.checkPbs();
    expect(app.pbsCheck).toEqual({ state: "error", message: "boom" });
  });
});
