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
  const stubCancel = (status: number, code: string, message: string) =>
    vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
      const path = new URL(url, "http://localhost/").pathname.replace(/^\/api\/v1/, "");
      if (path === "/status" || init?.method !== "POST") {
        const w = new World("running", "full");
        return new Response(JSON.stringify(w.handle("GET", path, new URLSearchParams(), null).body), { status: 200 });
      }
      return new Response(JSON.stringify({ code, message }), { status });
    });

  it("a run that is only just starting (409 cannot_cancel from the daemon) says so", async () => {
    const w = new World("ok", "full");
    serve(w);
    await app.startRun();
    expect(app.status?.current_run).toBeTruthy();
    // Just started: the daemon has no process to signal yet and answers 409.
    await app.cancelRun();
    expect(app.cancel).toMatchObject({ state: "error", kind: "cannot" });
  });

  it("a 404 (an older daemon answered it for every case) keeps the broad text", async () => {
    stubCancel(404, "not_found", "no running backup with this id");
    await app.poll();
    await app.cancelRun();
    expect(app.cancel).toMatchObject({ state: "error", kind: "gone" });
  });

  it("a 409 from a stub daemon is 'cannot'", async () => {
    stubCancel(409, "cannot_cancel", "this backup has already finished and cannot be cancelled");
    await app.poll();
    await app.cancelRun();
    expect(app.cancel).toMatchObject({ state: "error", kind: "cannot" });
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

  it("does not wake the server by itself: the first check is asked without wake", async () => {
    const urls: string[] = [];
    const w = new World("overdue", "full");
    vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
      urls.push(url);
      const path = new URL(url, "http://localhost/").pathname.replace(/^\/api\/v1/, "");
      const r = w.handle(init?.method ?? "GET", path, new URL(url, "http://localhost/").searchParams, null);
      return new Response(JSON.stringify(r.body), { status: r.status });
    });
    await app.poll();
    expect(app.status?.pbs.wake_on_lan).toBe(true);
    await app.checkPbs();
    expect(urls.some((u) => u.includes("pbs/check") && !u.includes("wake"))).toBe(true);
    expect(urls.some((u) => u.includes("wake=true"))).toBe(false);
    expect(app.pbsCheck).toMatchObject({ state: "done", wake: false });
    await app.checkPbs(true);
    expect(urls.some((u) => u.includes("pbs/check?wake=true"))).toBe(true);
    expect(app.pbsCheck).toMatchObject({ state: "done", wake: true });
  });

  it("an answer that is not JSON-ok (daemon error) is an error with the daemon's message", async () => {
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ code: "internal", message: "boom" }), { status: 500 }));
    await app.checkPbs();
    expect(app.pbsCheck).toEqual({ state: "error", message: "boom" });
  });
});

describe("other computers (GET /machines)", () => {
  afterEach(() => {
    app.machines = null;
    app.machinesError = null;
  });

  it("a server that does not answer is an error of the list (the sidebar hint), a working one is not", async () => {
    serve(new World("pbs_unreachable", "full"));
    await app.loadMachines();
    expect(app.machinesError).toMatchObject({ status: 503 });

    serve(new World("ok", "full"));
    await app.loadMachines();
    expect(app.machinesError).toBeNull();
    expect(app.machines?.machines.length).toBeGreaterThan(0);
  });

  it("an older daemon without the endpoint is no error", async () => {
    serve(new World("pbs_unreachable", "today"));
    await app.loadMachines();
    expect(app.machinesError).toBeNull();
    expect(app.machines).toBeFalsy();
  });
});
