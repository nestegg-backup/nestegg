// Thin client for the local API. Relative URLs only: the page is served by
// `nestegg ui` (or the Vite dev server). Every request carries the session
// token as `Authorization: Bearer …` (see session.ts).

import { authHeaders } from "./session";

import type {
  ConfigView,
  DirEntry,
  HealthInfo,
  MachinesResponse,
  PbsCheckResult,
  PbsInfo,
  Restore,
  RestoreRequest,
  Run,
  RunDetail,
  SessionInfo,
  SettingsChange,
  Snapshot,
  Status,
  Target,
} from "./types";

export type FailureKind =
  | "unauthorized" // 401: no or wrong session token; the session has ended
  | "daemon" // 502 from `nestegg ui` or network error: daemon not reachable
  | "not_implemented" // 501
  | "not_found" // 404
  | "conflict" // 409
  | "pbs_unreachable" // 503
  | "bad_request" // 400
  | "other";

export class ApiFailure extends Error {
  constructor(
    readonly kind: FailureKind,
    readonly status: number,
    readonly code: string | null,
    message: string,
  ) {
    super(message);
  }
}

function kindOf(status: number, code: string | null): FailureKind {
  if (status === 401) return "unauthorized";
  if (status === 501 || code === "not_implemented") return "not_implemented";
  if (status === 503 || code === "pbs_unreachable") return "pbs_unreachable";
  if (status === 502) return "daemon";
  if (status === 404) return "not_found";
  if (status === 409) return "conflict";
  if (status === 400) return "bad_request";
  return "other";
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`api/v1/${path}`, {
      credentials: "omit",
      cache: "no-store",
      ...init,
      headers: {
        Accept: "application/json",
        ...(init?.body ? { "Content-Type": "application/json" } : {}),
        ...authHeaders(),
      },
    });
  } catch (e) {
    throw new ApiFailure("daemon", 0, null, e instanceof Error ? e.message : String(e));
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
    throw new ApiFailure(kindOf(res.status, code), res.status, code, message);
  }
  return (await res.json()) as T;
}

const q = (s: string) => encodeURIComponent(s);

export const api = {
  health: () => request<HealthInfo>("health"),
  status: () => request<Status>("status"),
  runs: (limit = 200) => request<{ runs: Run[] }>(`runs?limit=${limit}`).then((r) => r.runs),
  run: (id: string) => request<RunDetail>(`runs/${q(id)}`),
  /** All configured targets (the stick first, then the backup server). */
  startRun: () => request<Run>("runs", { method: "POST" }),
  /** Up to 200 runs of one target, newest first, including the active run. Older records without a target belong to the server. */
  targetRuns: (target: Target) => request<{ runs: Run[] }>(`targets/${target}/runs`).then((r) => r.runs),
  /**
   * Back up to this target only. 202 means started: the run then shows up as `current_run`; a stick
   * that is not plugged in ends that run as `stick_not_plugged_in`. 409: a backup is already running.
   */
  startTargetRun: (target: Target) => request<Run>(`targets/${target}/runs`, { method: "POST" }),
  /** Ask the daemon to stop a running backup. Rejects with kind `conflict` (`cannot_cancel`: still starting, or already over) or `not_found` (unknown run; older daemons also answered 404 for the former). */
  cancelRun: (id: string) => request<{ cancelling?: boolean }>(`runs/${q(id)}/cancel`, { method: "POST" }),
  pbs: () => request<PbsInfo>("pbs"),
  /** Runs the checks of `nestegg check`; without `wake` a sleeping PBS is reported as not checked; with `wake` it is woken first (can take minutes). */
  checkPbs: (wake: boolean) => request<PbsCheckResult>(`pbs/check${wake ? "?wake=true" : ""}`, { method: "POST" }),
  snapshots: () => request<{ snapshots: Snapshot[] }>("snapshots").then((r) => r.snapshots),
  browse: (time: string, archive: string, path: string) =>
    request<{ entries: DirEntry[] }>(`snapshots/${q(time)}/browse?archive=${q(archive)}&path=${q(path)}`).then(
      (r) => r.entries,
    ),
  startRestore: (body: RestoreRequest) =>
    request<Restore>("restores", { method: "POST", body: JSON.stringify(body) }),
  restore: (id: string) => request<Restore>(`restores/${q(id)}`),
  config: () => request<ConfigView>("config"),
  /**
   * Change settings (only the given fields); resolves to the new configuration.
   * Validation errors reject with kind `bad_request` and the daemon's message.
   */
  updateConfig: (change: SettingsChange) =>
    request<ConfigView>("config", { method: "PATCH", body: JSON.stringify(change) }),
  // Older daemons do not offer these; a 404 means "not offered by this daemon" (see `optional`).
  machines: () => request<MachinesResponse>("machines"),
  session: () => request<SessionInfo>("session"),
};

/** Undefined when the endpoint does not exist (404/501), the value otherwise. */
export async function optional<T>(p: Promise<T>): Promise<T | undefined> {
  try {
    return await p;
  } catch (e) {
    if (e instanceof ApiFailure && (e.kind === "not_found" || e.kind === "not_implemented")) return undefined;
    throw e;
  }
}
