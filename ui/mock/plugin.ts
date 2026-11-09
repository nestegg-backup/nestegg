// Vite dev-server plugin that answers /api/v1/* from the mock world.
//
// Auth works like `nestegg ui`: every /api/ request needs
// `Authorization: Bearer <token>`, otherwise 401. The page gets the token from
// the address, so open the dev server as
//   http://127.0.0.1:5173/#token=dev
// (token `dev`, or the value of NESTEGG_MOCK_TOKEN). The address is printed
// when the dev server starts.
//
// Choose a scenario with `?scenario=<name>` before the fragment, e.g.
//   http://127.0.0.1:5173/?scenario=failing#token=dev
// optionally with `&daemon=today` and `&fault=a,b` (see world.ts). Loading
// the page with `?scenario=` makes that scenario the current one (for all
// tabs) and resets it. No cookies are used.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";
import { FAULTS, SCENARIOS, World, type Fault, type Flavor, type Scenario } from "./world.ts";

export const MOCK_TOKEN = process.env.NESTEGG_MOCK_TOKEN || "dev";

/** Same rule as `nestegg ui`: a Bearer header with exactly the session token. */
export function authorized(header: string | string[] | undefined, token: string): boolean {
  if (typeof header !== "string" || !header.startsWith("Bearer ")) return false;
  return header.slice("Bearer ".length) === token;
}

const UNAUTHORIZED = "Not authorized. Open the address printed by `nestegg ui` in your terminal.\n";

let current = new World("ok", "full");

function pick(value: string | null | undefined): Scenario {
  return (SCENARIOS as readonly string[]).includes(value ?? "") ? (value as Scenario) : "ok";
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
      server.httpServer?.once("listening", () => {
        const a = server.httpServer?.address();
        const port = a && typeof a === "object" ? a.port : 5173;
        server.config.logger.info(`  Mock API: open http://127.0.0.1:${port}/#token=${MOCK_TOKEN}`);
      });
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        // Page load with ?scenario=...: make it the current scenario and reset it.
        if (!url.pathname.startsWith("/api/") && url.searchParams.has("scenario")) {
          const flavor: Flavor = url.searchParams.get("daemon") === "today" ? "today" : "full";
          current = new World(pick(url.searchParams.get("scenario")), flavor);
          for (const fault of (url.searchParams.get("fault") ?? "").split(",")) {
            if ((FAULTS as readonly string[]).includes(fault)) current.faults.add(fault as Fault);
          }
          return next();
        }
        // Switch faults at runtime, e.g. to stop the "daemon" after the page has loaded.
        if (url.pathname === "/__mock/fault" && req.method === "POST") {
          for (const fault of FAULTS) {
            const v = url.searchParams.get(fault);
            if (v === "1") current.faults.add(fault);
            if (v === "0") current.faults.delete(fault);
          }
          return send(res, 200, { faults: [...current.faults] });
        }
        if (!url.pathname.startsWith("/api/")) return next();
        if (!authorized(req.headers.authorization, MOCK_TOKEN)) return send(res, 401, UNAUTHORIZED);
        const body = req.method === "POST" || req.method === "PATCH" ? await readBody(req) : null;
        const reply = current.handle(req.method ?? "GET", url.pathname.slice("/api/v1".length), url.searchParams, body);
        // A little latency, so loading states are visible during development.
        setTimeout(() => send(res, reply.status, reply.body), 120);
      });
    },
  };
}
