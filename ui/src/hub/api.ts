// Thin client for the hub API (`api/hub-openapi.yaml`, base `/api/hub/v1`).
//
// The hub serves this page at `/`, so the relative URL `api/hub/v1/...` is the
// API. The administrator's session is a cookie (`HttpOnly`, set by
// `POST /session`); the browser sends it by itself. Every request that is not
// GET/HEAD carries `X-Nestegg-Hub: 1`: a custom header cannot be sent
// cross-site without a CORS preflight, which the hub never answers.

import type { HubInfo, HubMachines, JoinCode, JoinCodeInfo } from "./types";

export const HUB_HEADER = "X-Nestegg-Hub";

export class HubFailure extends Error {
  /**
   * `status` 0: no answer at all (network, hub stopped). `code` is the stable code of the
   * `Error` schema (`unauthorized`, `rate_limited`, `master_key_unproven`, `no_package`,
   * `pbs_unreachable`, `pbs_refused`, `not_found`, ...), or null when the body had none.
   */
  constructor(
    readonly status: number,
    readonly code: string | null,
    message: string,
  ) {
    super(message);
  }

  get unauthorized(): boolean {
    return this.status === 401;
  }
}

export function asHubFailure(e: unknown): HubFailure {
  return e instanceof HubFailure ? e : new HubFailure(0, null, e instanceof Error ? e.message : String(e));
}

type Fetch = typeof fetch;

/** Called on every 401 except for the login itself: the session has ended. */
let onUnauthorized: () => void = () => {};
export function setUnauthorizedHandler(fn: () => void) {
  onUnauthorized = fn;
}

export async function hubRequest<T>(path: string, init: RequestInit = {}, fetchFn: Fetch = fetch): Promise<T> {
  const method = (init.method ?? "GET").toUpperCase();
  const headers: Record<string, string> = { Accept: "application/json" };
  if (init.body) headers["Content-Type"] = "application/json";
  if (method !== "GET" && method !== "HEAD") headers[HUB_HEADER] = "1";
  let res: Response;
  try {
    res = await fetchFn(`api/hub/v1/${path}`, { credentials: "same-origin", cache: "no-store", ...init, method, headers });
  } catch (e) {
    throw new HubFailure(0, null, e instanceof Error ? e.message : String(e));
  }
  if (!res.ok) {
    let code: string | null = null;
    let message = res.statusText;
    const text = await res.text().catch(() => "");
    try {
      const body = JSON.parse(text) as { code?: unknown; message?: unknown };
      if (typeof body.code === "string") code = body.code;
      if (typeof body.message === "string") message = body.message;
    } catch {
      if (text.trim()) message = text.trim();
    }
    const failure = new HubFailure(res.status, code, message);
    if (res.status === 401 && path !== "session") onUnauthorized();
    throw failure;
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

const q = (s: string) => encodeURIComponent(s);

export const hubApi = {
  session: () => hubRequest<{ logged_in: boolean }>("session"),
  /** Resolves when logged in. Rejects with status 401 (wrong password) or 429 (`rate_limited`). */
  login: (password: string) => hubRequest<void>("session", { method: "POST", body: JSON.stringify({ password }) }),
  logout: () => hubRequest<void>("session", { method: "DELETE" }),
  hub: () => hubRequest<HubInfo>("hub"),
  machines: () => hubRequest<HubMachines>("machines"),
  /** 503: the PBS did not answer (nothing was changed); 404: already gone. */
  removeMachine: (id: string) => hubRequest<void>(`machines/${q(id)}`, { method: "DELETE" }),
  joinCodes: () => hubRequest<{ codes: JoinCodeInfo[] }>("join-codes").then((r) => r.codes),
  /** 409 `master_key_unproven` or `no_package`: refused, with an instruction for the hub's terminal. */
  createJoinCode: (note: string) =>
    hubRequest<JoinCode>("join-codes", { method: "POST", body: JSON.stringify(note.trim() ? { note: note.trim() } : {}) }),
  revokeJoinCode: (id: string) => hubRequest<void>(`join-codes/${q(id)}`, { method: "DELETE" }),
};
