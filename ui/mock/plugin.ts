// Vite dev-server plugin that answers /api/v1/* from the mock world.
//
// Choose a scenario by opening the page with `?scenario=<name>` (and
// optionally `&daemon=today` to get only what the real daemon offers today).
// The choice is kept in a cookie, so API calls of the page use it too.
// Opening the page with `?scenario=` again resets that scenario.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";
import { FAULTS, SCENARIOS, World, type Fault, type Flavor, type Scenario } from "./world.ts";

const worlds = new Map<string, World>();

function cookies(req: IncomingMessage): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of (req.headers.cookie ?? "").split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k) out[k] = decodeURIComponent(v.join("="));
  }
  return out;
}

function pick(value: string | null | undefined): Scenario {
  return (SCENARIOS as readonly string[]).includes(value ?? "") ? (value as Scenario) : "ok";
}

function world(scenario: Scenario, flavor: Flavor, reset = false): World {
  const key = `${scenario}/${flavor}`;
  let w = worlds.get(key);
  if (!w || reset) {
    w = new World(scenario, flavor);
    worlds.set(key, w);
  }
  return w;
}

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const text = Buffer.concat(chunks).toString("utf8");
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function send(res: ServerResponse, status: number, body: unknown) {
  res.statusCode = status;
  res.setHeader("Cache-Control", "no-store");
  if (typeof body === "string") {
    res.setHeader("Content-Type", "text/plain; charset=utf-8");
    res.end(body);
  } else {
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify(body));
  }
}

export function mockApi(): Plugin {
  return {
    name: "nestegg-mock-api",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        // Page load with ?scenario=...: remember the choice and reset that world.
        if (!url.pathname.startsWith("/api/") && url.searchParams.has("scenario")) {
          const scenario = pick(url.searchParams.get("scenario"));
          const flavor: Flavor = url.searchParams.get("daemon") === "today" ? "today" : "full";
          const w = world(scenario, flavor, true);
          for (const fault of (url.searchParams.get("fault") ?? "").split(",")) {
            if ((FAULTS as readonly string[]).includes(fault)) w.faults.add(fault as Fault);
          }
          res.setHeader("Set-Cookie", [
            `mock_scenario=${scenario}; Path=/; SameSite=Strict`,
            `mock_daemon=${flavor}; Path=/; SameSite=Strict`,
          ]);
          return next();
        }
        const c = cookies(req);
        // Switch faults at runtime, e.g. to stop the "daemon" after the page has loaded.
        if (url.pathname === "/__mock/fault" && req.method === "POST") {
          const w = world(pick(c.mock_scenario), c.mock_daemon === "today" ? "today" : "full");
          for (const fault of FAULTS) {
            const v = url.searchParams.get(fault);
            if (v === "1") w.faults.add(fault);
            if (v === "0") w.faults.delete(fault);
          }
          return send(res, 200, { faults: [...w.faults] });
        }
        if (!url.pathname.startsWith("/api/v1/")) return next();
        const scenario = pick(url.searchParams.get("scenario") ?? c.mock_scenario);
        const flavor: Flavor = (url.searchParams.get("daemon") ?? c.mock_daemon) === "today" ? "today" : "full";
        const body = req.method === "POST" ? await readBody(req) : null;
        const reply = world(scenario, flavor).handle(req.method ?? "GET", url.pathname.slice("/api/v1".length), url.searchParams, body);
        // A little latency, so loading states are visible during development.
        setTimeout(() => send(res, reply.status, reply.body), 120);
      });
    },
  };
}
