// Mock of the hub API (`api/hub-openapi.yaml`) for developing the hub page.
// Placeholder data only (see ../../AGENTS.md): hub.example.com, pbs.example.com,
// <FINGERPRINT>, generic computer names.
//
// Like the hub, the mock keeps a session cookie (`nestegg_hub_session`) and
// refuses requests that are not GET/HEAD without `X-Nestegg-Hub: 1`. Times are
// relative to "now". A code created through the page is "used" by a made-up
// computer after a while, which then shows up in the list without a snapshot,
// and a little later with its first backup.

import type { HubInfo, HubMachine, HubMachines, JoinCode, JoinCodeInfo, UnmanagedGroup } from "../src/hub/types.ts";

export const HUB_SCENARIOS = [
  /** Several computers in different states, one group without a computer, one open code. */
  "ok",
  /** The first visit: not logged in (the password is `hub`). */
  "logged_out",
  /** Logged in, no computer has joined yet. */
  "empty",
  /** The PBS did not answer; the list is what the hub read 9 hours ago. */
  "pbs_asleep",
  /** The hub has never been able to read the PBS. */
  "pbs_never",
  /** `hub init` without the master key proof: "Add computer" is refused with `master_key_unproven`. */
  "master_key_unproven",
  /** The hub has no package to hand out: refused with `no_package`. */
  "no_package",
  /** A created code is used after about 8 seconds (otherwise about 20). */
  "code_created",
  /** Logged out, and every login answers 429 `rate_limited`. */
  "rate_limited",
  /** Removing a computer and creating a code fail (`remove` and `create` below). */
  "errors",
] as const;
export type HubScenario = (typeof HUB_SCENARIOS)[number];

/**
 * Faults, `&fault=a,b` on the page address or at runtime with `POST /__mock/hub?<fault>=1|0`:
 * - `down`: the hub stops answering (the connection is closed, as with a stopped hub)
 * - `machines`: `GET /machines` answers 500
 * - `create`: `POST /join-codes` answers 500
 * - `remove`: `DELETE /machines/{id}` answers 503 `pbs_unreachable` (nothing changes)
 * - `refused`: `DELETE /machines/{id}` answers 503 `pbs_refused` with the PBS's text
 * - `no_join`: a created code is never used
 * - `short_codes`: new codes are valid for 70 seconds (to see a code expire)
 */
export const HUB_FAULTS = ["down", "machines", "create", "remove", "refused", "no_join", "short_codes"] as const;
export type HubFault = (typeof HUB_FAULTS)[number];

export const HUB_PASSWORD = process.env.NESTEGG_MOCK_HUB_PASSWORD || "hub";
export const SESSION_COOKIE = "nestegg_hub_session";

export interface HubReply {
  status: number;
  body: unknown;
  delayMs?: number;
  /** `Set-Cookie` header value. */
  cookie?: string;
  /** Close the connection without an answer (fault `down`). */
  drop?: boolean;
}

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const CODE_ALPHABET = "23456789ACDEFGHJKMNPQRTUVWXY";
const PIN = "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const JOINING_NAMES = ["laptop-2", "workstation", "notebook", "living-room", "attic"];

const iso = (ms: number) => new Date(ms).toISOString();
const err = (status: number, code: string, message: string, delayMs?: number): HubReply => ({ status, body: { code, message }, delayMs });

function cookieOf(header: string | undefined, name: string): string | null {
  for (const part of (header ?? "").split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === name) return v.join("=");
  }
  return null;
}

/** The mock serves plain http on loopback: no `Secure` (the real hub sets it). */
export function sessionCookie(value: string | null): string {
  return value
    ? `${SESSION_COOKIE}=${value}; HttpOnly; SameSite=Strict; Path=/`
    : `${SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0`;
}

interface PendingJoin {
  codeId: string;
  joinAt: number;
}

export class HubWorld {
  info: HubInfo;
  machines: HubMachine[] = [];
  unmanaged: UnmanagedGroup[] = [];
  codes: JoinCodeInfo[] = [];
  pbsReachable = true;
  pbsReadAt: number | null;
  faults = new Set<HubFault>();
  sessions = new Set<string>();
  private pending: PendingJoin[] = [];
  /** Computers that joined through the page: their first snapshot appears at this time. */
  private firstBackupAt = new Map<string, number>();
  private wrongLogins = 0;
  private lockedUntil = 0;
  private seq = 0;

  constructor(
    readonly scenario: HubScenario,
    now = Date.now(),
  ) {
    this.pbsReadAt = now - 2 * MIN;
    this.info = {
      name: "Home",
      version: "0.1.0",
      url: "https://hub.example.com:8443",
      pin: PIN,
      pbs: { server: "pbs.example.com", port: 8007, datastore: "backups", namespace: "home", user: "nestegg-hub@pbs", wake_on_lan: true },
      master_key: { present: true, proven: true, proven_at: iso(now - 30 * DAY), fingerprint: "<FINGERPRINT>" },
      package: { available: true, version: "0.1.0", architecture: "amd64" },
    };
    if (scenario === "errors") {
      this.faults.add("remove");
      this.faults.add("create");
    }
    if (scenario === "master_key_unproven") this.info.master_key = { present: true, proven: false, proven_at: null, fingerprint: "<FINGERPRINT>" };
    if (scenario === "no_package") this.info.package = { available: false, version: null, architecture: null };
    if (scenario === "empty" || scenario === "master_key_unproven") return;
    this.machines = this.household(now);
    this.unmanaged = [{ backup_id: "old-laptop", last_snapshot: iso(now - 94 * DAY) }];
    this.codes = [{ code_id: "c-open", note: "spare laptop", created_at: iso(now - 10 * MIN), expires_at: iso(now + 20 * MIN) }];
    switch (scenario) {
      case "pbs_asleep":
        this.pbsReachable = false;
        this.pbsReadAt = now - 9 * HOUR;
        break;
      case "pbs_never":
        this.pbsReachable = false;
        this.pbsReadAt = null;
        this.unmanaged = [];
        // Nothing was ever read from the PBS: no snapshots, no reports.
        this.machines = this.machines.slice(0, 3).map((m) => ({ ...m, snapshot_count: 0, last_snapshot: null, last_snapshot_verified: null, report: null }));
        break;
      default:
        break;
    }
  }

  /** Called from the constructor, after the field initialisers. */
  private household(now: number): HubMachine[] {
    const report = (at: number, extra: Partial<NonNullable<HubMachine["report"]>> = {}) => ({
      reported_at: iso(at + 25 * MIN),
      result: "succeeded" as const,
      run_started_at: iso(at),
      key_exported: false,
      overdue_after_hours: 36,
      nestegg_version: "0.1.0",
      sources: ["/home", "/etc"],
      verified: true,
      ...extra,
    });
    const nightly = (days: number) => now - days * DAY - 3 * HOUR;
    return [
      {
        machine_id: "m-desktop", backup_id: "desktop", hostname: "desktop", os: "Linux Mint 22.2", nestegg_version: "0.1.0",
        joined_at: iso(now - 61 * DAY), snapshot_count: 58, last_snapshot: iso(nightly(0)), last_snapshot_verified: true,
        report: report(nightly(0)),
      },
      {
        machine_id: "m-laptop", backup_id: "laptop", hostname: "laptop", os: "Ubuntu 24.04", nestegg_version: "0.1.0",
        joined_at: iso(now - 40 * DAY), snapshot_count: 31, last_snapshot: iso(nightly(4)), last_snapshot_verified: true,
        report: report(nightly(4)),
      },
      {
        machine_id: "m-media", backup_id: "media", hostname: "media-pc", os: "Debian 13", nestegg_version: "0.1.0",
        joined_at: iso(now - 40 * DAY), snapshot_count: 25, last_snapshot: iso(nightly(1)), last_snapshot_verified: null,
        report: {
          reported_at: iso(now - 10 * HOUR + 2 * MIN), result: "failed", run_started_at: iso(now - 10 * HOUR),
          error: { kind: "source_missing", message: "source /srv/media does not exist" }, key_exported: false, overdue_after_hours: 36,
          nestegg_version: "0.1.0",
        },
      },
      {
        machine_id: "m-office", backup_id: "office", hostname: "office", os: "Fedora 43", nestegg_version: "0.1.0",
        joined_at: iso(now - 20 * DAY), snapshot_count: 19, last_snapshot: iso(nightly(0) + 20 * MIN), last_snapshot_verified: null,
        report: report(nightly(0) + 20 * MIN, { incomplete: ["optional source /mnt/archive is not mounted; skipped"] }),
      },
      {
        machine_id: "m-study", backup_id: "study", hostname: "study", os: "Linux Mint 22.2", nestegg_version: "0.1.0",
        joined_at: iso(now - 12 * DAY), snapshot_count: 11, last_snapshot: iso(nightly(0) - 40 * MIN), last_snapshot_verified: false,
        report: report(nightly(0) - 40 * MIN),
      },
      {
        machine_id: "m-kids", backup_id: "kids-pc", hostname: "kids-pc", os: "Ubuntu 24.04", nestegg_version: "0.1.0",
        joined_at: iso(now - 2 * HOUR), snapshot_count: 0, last_snapshot: null, last_snapshot_verified: null, report: null,
      },
    ];
  }

  /** A session for a page load of a scenario that starts logged in. */
  openSession(): string | null {
    if (this.scenario === "logged_out" || this.scenario === "rate_limited") return null;
    return this.newSession();
  }

  private newSession(): string {
    const id = `mock-${++this.seq}-${Math.random().toString(36).slice(2, 10)}`;
    this.sessions.add(id);
    return id;
  }

  private newCode(): string {
    let s = "";
    for (let i = 0; i < 12; i++) s += CODE_ALPHABET[Math.floor(Math.random() * CODE_ALPHABET.length)];
    return `${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}`;
  }

  /** Let time pass: codes expire, pending joins happen, first backups arrive. */
  private advance(now: number) {
    this.codes = this.codes.filter((c) => new Date(c.expires_at).getTime() > now);
    for (const p of [...this.pending]) {
      const code = this.codes.find((c) => c.code_id === p.codeId);
      if (!code) {
        this.pending = this.pending.filter((x) => x !== p);
        continue;
      }
      if (p.joinAt > now || this.faults.has("no_join")) continue;
      this.pending = this.pending.filter((x) => x !== p);
      this.codes = this.codes.filter((c) => c !== code);
      const taken = new Set(this.machines.map((m) => m.backup_id));
      const name = JOINING_NAMES.find((n) => !taken.has(n)) ?? `computer-${this.seq}`;
      const id = `m-${name}-${++this.seq}`;
      this.machines.push({
        machine_id: id, backup_id: name, hostname: name, os: "Linux Mint 22.2", nestegg_version: "0.1.0",
        joined_at: iso(p.joinAt), snapshot_count: 0, last_snapshot: null, last_snapshot_verified: null, report: null,
      });
      this.firstBackupAt.set(id, p.joinAt + 40_000);
    }
    for (const [id, at] of [...this.firstBackupAt]) {
      if (at > now || !this.pbsReachable) continue;
      this.firstBackupAt.delete(id);
      const m = this.machines.find((x) => x.machine_id === id);
      if (!m) continue;
      m.snapshot_count = 1;
      m.last_snapshot = iso(at - 30_000);
      m.report = { reported_at: iso(at), result: "succeeded", run_started_at: iso(at - 30_000), key_exported: false, overdue_after_hours: 36, nestegg_version: "0.1.0", verified: true };
    }
    if (this.pbsReachable) this.pbsReadAt = now;
  }

  handle(method: string, path: string, body: unknown, headers: { cookie?: string; hubHeader?: string }, now = Date.now()): HubReply {
    if (this.faults.has("down")) return { status: 0, body: null, drop: true };
    const m = method.toUpperCase();
    if (m !== "GET" && m !== "HEAD" && headers.hubHeader !== "1") {
      return err(400, "bad_request", "requests that change something need the header X-Nestegg-Hub: 1");
    }
    const session = cookieOf(headers.cookie, SESSION_COOKIE);
    const loggedIn = !!session && this.sessions.has(session);

    if (path === "/session") {
      if (m === "GET") return { status: 200, body: { logged_in: loggedIn } };
      if (m === "DELETE") {
        if (session) this.sessions.delete(session);
        return { status: 204, body: null, cookie: sessionCookie(null) };
      }
      if (m === "POST") return this.login(body, now);
    }
    if (!loggedIn) return err(401, "unauthorized", "not logged in");
    this.advance(now);

    if (path === "/hub" && m === "GET") return { status: 200, body: this.info };
    if (path === "/machines" && m === "GET") {
      if (this.faults.has("machines")) return err(500, "internal", "reading the hub database failed");
      const res: HubMachines = {
        pbs_reachable: this.pbsReachable,
        pbs_read_at: this.pbsReadAt === null ? null : iso(this.pbsReadAt),
        machines: this.machines,
        unmanaged: this.unmanaged,
      };
      return { status: 200, body: res, delayMs: this.pbsReachable ? 250 : 120 };
    }
    const machine = /^\/machines\/([^/]+)$/.exec(path);
    if (machine && m === "DELETE") {
      const id = decodeURIComponent(machine[1]!);
      if (!this.machines.some((x) => x.machine_id === id)) return err(404, "not_found", "no such computer");
      if (this.faults.has("remove") || !this.pbsReachable) return err(503, "pbs_unreachable", "the backup server did not answer", 1500);
      if (this.faults.has("refused")) return err(503, "pbs_refused", "permission check failed (403)", 600);
      this.machines = this.machines.filter((x) => x.machine_id !== id);
      return { status: 204, body: null, delayMs: 600 };
    }
    if (path === "/join-codes" && m === "GET") return { status: 200, body: { codes: this.codes } };
    if (path === "/join-codes" && m === "POST") return this.createCode(body, now);
    const code = /^\/join-codes\/([^/]+)$/.exec(path);
    if (code && m === "DELETE") {
      const id = decodeURIComponent(code[1]!);
      if (!this.codes.some((c) => c.code_id === id)) return err(404, "not_found", "no such code");
      this.codes = this.codes.filter((c) => c.code_id !== id);
      return { status: 204, body: null };
    }
    return err(404, "not_found", `no route ${m} ${path}`);
  }

  private login(body: unknown, now: number): HubReply {
    if (this.scenario === "rate_limited" || now < this.lockedUntil) return err(429, "rate_limited", "too many failed logins", 400);
    const password = body && typeof body === "object" ? (body as { password?: unknown }).password : undefined;
    if (password !== HUB_PASSWORD) {
      // Like the hub: a wrong password is answered after a delay; repeated failures are slowed down further.
      if (++this.wrongLogins >= 3) this.lockedUntil = now + 60_000;
      return err(401, "unauthorized", "wrong password", 800);
    }
    this.wrongLogins = 0;
    return { status: 204, body: null, cookie: sessionCookie(this.newSession()), delayMs: 300 };
  }

  private createCode(body: unknown, now: number): HubReply {
    if (!this.info.master_key.proven) {
      return err(409, "master_key_unproven", "the copy of the master key has not been confirmed: run `nestegg hub master-key confirm`");
    }
    if (!this.info.package.available) return err(409, "no_package", "the hub has no nestegg package to hand out");
    if (this.faults.has("create")) return err(500, "internal", "writing the hub database failed", 400);
    const raw = body && typeof body === "object" ? (body as { note?: unknown }).note : undefined;
    if (raw !== undefined && (typeof raw !== "string" || raw.length > 80)) return err(400, "bad_request", "note: at most 80 characters");
    const note = typeof raw === "string" && raw.trim() ? raw.trim() : null;
    const valid = this.faults.has("short_codes") ? 70_000 : 30 * MIN;
    const code = this.newCode();
    const info: JoinCodeInfo = { code_id: `c-${++this.seq}`, note, created_at: iso(now), expires_at: iso(now + valid) };
    this.codes.push(info);
    this.pending.push({ codeId: info.code_id, joinAt: now + (this.scenario === "code_created" ? 8_000 : 20_000) });
    const res: JoinCode = {
      ...info,
      code,
      command: `curl -fsSk --pinnedpubkey '${PIN}' ${this.info.url}/join.sh | sudo sh -s -- ${code}`,
    };
    return { status: 201, body: res, delayMs: 400 };
  }
}

/** The world of a page load with `?scenario=` (and `&fault=`). */
export function hubWorldFor(params: URLSearchParams, now = Date.now()): HubWorld {
  const name = params.get("scenario") ?? "ok";
  const scenario = (HUB_SCENARIOS as readonly string[]).includes(name) ? (name as HubScenario) : "ok";
  const w = new HubWorld(scenario, now);
  for (const f of (params.get("fault") ?? "").split(",")) {
    if ((HUB_FAULTS as readonly string[]).includes(f)) w.faults.add(f as HubFault);
  }
  return w;
}
