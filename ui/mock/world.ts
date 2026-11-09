// Mock of the nestegg daemon API for UI development. Placeholder data only
// (see ../../AGENTS.md): desktop, pbs.example.com, /home/alice, 00:00:5e:00:53:01.
//
// A "world" is the in-memory state of one scenario. Times are generated
// relative to "now", so the 14-day strip always looks realistic.

import type {
  ConfigView,
  DirEntry,
  Machine,
  MachinesResponse,
  Restore,
  RestoreRequest,
  Run,
  RunDetail,
  Snapshot,
  Status,
} from "../src/lib/types.ts";

export const SCENARIOS = [
  "ok",
  "running",
  "overdue",
  "failing",
  "never",
  "unconfigured",
  "pbs_unreachable",
  "key_not_exported",
  "credentials_unreadable",
  "daemon_down",
  "unauthorized",
] as const;
export type Scenario = (typeof SCENARIOS)[number];

/** `full`: all planned endpoints. `today`: only what the real daemon offers today (browse/restore 501, no machines/session). */
export type Flavor = "full" | "today";

/**
 * Faults that can be added to any scenario (`&fault=a,b` on the page URL):
 * - `down`: the daemon stops answering (502), e.g. after the page has loaded
 *   (toggle at runtime with `POST /__mock/fault?down=1|0`)
 * - `runs`: `/runs` fails with 500
 * - `config`: `/config` fails with 500
 * - `schedule_off`: the timer is disabled
 * - `no_run_details`: `/status` omits last_run and last_success (allowed by the contract)
 */
export const FAULTS = ["down", "runs", "config", "schedule_off", "no_run_details"] as const;
export type Fault = (typeof FAULTS)[number];

export interface Reply {
  status: number;
  body: unknown;
}

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const GB = 1_000_000_000;

const iso = (ms: number) => new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");

/** Deterministic pseudo-random numbers, so a scenario looks the same on every reload. */
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

/** Local midnight of the day `daysAgo` days before `now`, plus hours/minutes. */
function localAt(now: number, daysAgo: number, h: number, m: number): number {
  const d = new Date(now);
  d.setDate(d.getDate() - daysAgo);
  d.setHours(h, m, Math.floor((m * 7) % 60), 0);
  return d.getTime();
}

const ARCHIVES = [
  { name: "home.pxar", source: "/home" },
  { name: "etc.pxar", source: "/etc" },
  { name: "root.pxar", source: "/root" },
  { name: "usr-local.pxar", source: "/usr/local" },
];

interface StoredRun extends Run {
  log_tail: string[];
}

interface ActiveRun {
  run: StoredRun;
  startedMs: number;
  outcome: "succeeded" | "pbs_unreachable" | "config";
}

interface RestoreJob {
  restore: Restore;
  startedMs: number;
}

const fmtClock = (ms: number) => new Date(ms).toTimeString().slice(0, 8);

function logFor(run: Run, startMs: number): string[] {
  const t = (offsetS: number) => fmtClock(startMs + offsetS * 1000);
  const head = `${t(0)}  start: trigger=${run.trigger} backup-id=desktop`;
  switch (run.state) {
    case "succeeded":
      return [
        head,
        `${t(1)}  pbs.example.com:8007 reachable`,
        `${t(4)}  inventory written (412 packages, 38 units)`,
        `${t(5)}  proxmox-backup-client backup home.pxar:/home etc.pxar:/etc root.pxar:/root usr-local.pxar:/usr/local inventory.pxar`,
        ...(run.warnings ?? []).map((w) => `${t(200)}  warning: ${w}`),
        `${t((run.duration_seconds ?? 600) - 3)}  uploaded ${((run.uploaded_bytes ?? 0) / GB).toFixed(2)} GB`,
        `${t((run.duration_seconds ?? 600) - 1)}  status report uploaded`,
        `${t(run.duration_seconds ?? 600)}  result: succeeded (${run.snapshot})`,
      ];
    case "pbs_unreachable":
      return [
        head,
        `${t(0)}  pbs.example.com:8007 did not answer ping`,
        `${t(0)}  sent Wake-on-LAN packet to 00:00:5e:00:53:01`,
        `${t(300)}  no answer after 300 s, giving up`,
        `${t(300)}  result: pbs_unreachable`,
      ];
    case "failed":
      return [
        head,
        ...(run.error?.kind === "config" ? [] : [`${t(1)}  pbs.example.com:8007 reachable`]),
        `${t(66)}  ${run.error?.message ?? "error"}`,
        `${t(66)}  result: failed (${run.error?.kind ?? "internal"})`,
      ];
    default:
      return [head];
  }
}

function makeRun(
  startMs: number,
  state: Run["state"],
  opts: { trigger?: Run["trigger"]; duration?: number; uploaded?: number; error?: Run["error"]; warnings?: string[] } = {},
): StoredRun {
  const d = new Date(startMs);
  const stamp = d.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
  const trigger = opts.trigger ?? "timer";
  const duration = opts.duration ?? (state === "pbs_unreachable" ? 300 : state === "failed" ? 66 : 600);
  const run: Run = {
    id: `${stamp}-${trigger}`,
    trigger,
    state,
    started_at: iso(startMs),
    finished_at: iso(startMs + duration * 1000),
    duration_seconds: duration,
    snapshot: state === "succeeded" ? `host/desktop/${iso(startMs)}` : null,
    archives:
      state === "succeeded"
        ? [...ARCHIVES.map((a) => ({ ...a, size_bytes: a.name === "home.pxar" ? 380 * GB : 0.4 * GB })), { name: "inventory.pxar", source: "/var/lib/nestegg/inventory", size_bytes: 3_000_000 }]
        : [],
    uploaded_bytes: state === "succeeded" ? (opts.uploaded ?? GB) : null,
    warnings: opts.warnings ?? [],
    error: opts.error ?? null,
  };
  return { ...run, log_tail: logFor(run, startMs) };
}

// ------------------------------------------------------------------ file tree for browse

type Tree = { [name: string]: number | Tree };
const MB = 1_000_000;
const TREES: Record<string, Tree> = {
  "home.pxar": {
    alice: {
      Dokumente: {
        "Steuer 2025": { Belege: { "Beleg-01.pdf": 0.4 * MB, "Beleg-02.pdf": 0.3 * MB }, "Erklärung.pdf": 20 * MB },
        Verträge: { "Mietvertrag.pdf": 2 * MB, "Handy.pdf": 0.5 * MB },
        "Rezepte.odt": 4 * MB,
        "Lebenslauf.pdf": 1 * MB,
      },
      Bilder: {
        "2026": { "Urlaub Ostsee": { "IMG_0001.jpg": 6 * MB, "IMG_0002.jpg": 5.8 * MB }, Garten: { "IMG_0101.jpg": 4.2 * MB } },
        "2025": { "IMG_9001.jpg": 5.1 * MB },
        Familie: { "IMG_7001.jpg": 3.3 * MB },
      },
      Musik: { "playlist.m3u": 0.01 * MB },
      Downloads: { "installer.deb": 84 * MB },
      ".config": { "user-dirs.dirs": 0.001 * MB },
      ".bashrc": 0.004 * MB,
    },
  },
  "etc.pxar": { fstab: 900, hosts: 220, ssh: { sshd_config: 3300 }, apt: { "sources.list": 300 } },
  "root.pxar": { ".bashrc": 3100 },
  "usr-local.pxar": { bin: { "backup-hook.sh": 1200 }, share: {} },
  "inventory.pxar": { "packages.txt": 41_000, "manual-packages.txt": 3_900, "flatpaks.txt": 600, "units.txt": 2_400, "lsblk.json": 5_100, "os-release": 400 },
};

function browseTree(archive: string, path: string): DirEntry[] | null {
  let node: number | Tree | undefined = TREES[archive];
  for (const part of path.split("/").filter(Boolean)) {
    if (node === undefined || typeof node === "number") return null;
    node = node[part];
  }
  if (node === undefined || typeof node === "number") return null;
  return Object.entries(node).map(([name, v]) => ({
    name,
    type: typeof v === "number" ? "file" : "directory",
    size_bytes: typeof v === "number" ? Math.round(v) : null,
    modified: "2026-10-01T10:00:00Z",
  }));
}

/** Folders that "exist" in the home directory, to exercise the target-exists error. */
const EXISTING_TARGETS = ["Dokumente", "Bilder", "Musik", "Downloads", "nestegg-restore-2026-10-12"].map((n) => `/home/alice/${n}`);

// ------------------------------------------------------------------ the world

export class World {
  readonly createdMs: number;
  runs: StoredRun[] = [];
  active: ActiveRun | null = null;
  snapshots: Snapshot[] = [];
  restores = new Map<string, RestoreJob>();
  reachable: boolean | null = true;
  checkedMs: number | null = null;
  keyExported = true;
  configured = true;
  extraSources: { archive: string; path: string }[] = [];
  faults = new Set<Fault>();

  constructor(
    readonly scenario: Scenario,
    readonly flavor: Flavor,
    now = Date.now(),
  ) {
    this.createdMs = now;
    this.build(now);
  }

  private history(now: number, fromDaysAgo: number, toDaysAgo: number, opts: { skip?: number[]; pbsNight?: number } = {}) {
    const r = rng(42);
    for (let d = fromDaysAgo; d >= toDaysAgo; d--) {
      const minute = 31 + Math.floor(r() * 18);
      const start = localAt(now, d, 1, minute);
      if (start > now) continue;
      if (opts.skip?.includes(d)) continue;
      if (opts.pbsNight === d) {
        for (const h of [1, 2, 3]) this.runs.unshift(makeRun(localAt(now, d, h, 30), "pbs_unreachable"));
        continue;
      }
      const run = makeRun(start, "succeeded", {
        duration: 540 + Math.floor(r() * 480),
        uploaded: Math.round((0.4 + r() * 2.8) * GB),
        warnings: d === 4 ? ["/home/alice/.cache/thumbnails: permission denied, skipped"] : [],
      });
      this.runs.unshift(run);
    }
  }

  private build(now: number) {
    switch (this.scenario) {
      case "ok":
      case "running":
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        if (this.scenario === "running") this.beginRun(now - 6_000, "succeeded", "manual");
        break;
      case "overdue":
        this.history(now, 13, 3, { pbsNight: 7 });
        for (let d = 2; d >= 0; d--) {
          for (const h of [1, 2, 3, 9, 14]) {
            const at = localAt(now, d, h, 30);
            if (at < now) this.runs.unshift(makeRun(at, "pbs_unreachable"));
          }
        }
        this.reachable = false;
        break;
      case "failing":
        this.extraSources = [{ archive: "fotos.pxar", path: "/mnt/fotos" }];
        this.history(now, 13, 1, { skip: [10], pbsNight: 7 });
        // Before 01:31 local time today's attempt has not happened yet:
        // place the failure two hours ago instead of in the future.
        this.runs.unshift(
          makeRun(localAt(now, 0, 1, 31) < now ? localAt(now, 0, 1, 31) : now - 2 * HOUR, "failed", {
            error: { kind: "source_missing", message: "source /mnt/fotos: not a mount point (disk not mounted)" },
          }),
        );
        break;
      case "pbs_unreachable":
        // Last success a day ago, the attempts since then did not reach the server.
        this.history(now, 13, 2, { skip: [10], pbsNight: 7 });
        this.runs.unshift(makeRun(now - 26 * HOUR, "succeeded", { trigger: "manual", duration: 700, uploaded: 1.4 * GB }));
        for (const h of [2, 1]) this.runs.unshift(makeRun(now - h * HOUR, "pbs_unreachable"));
        this.reachable = false;
        break;
      case "never":
        this.reachable = null;
        break;
      case "unconfigured":
        this.configured = false;
        this.keyExported = false;
        this.reachable = null;
        break;
      case "key_not_exported":
        this.keyExported = false;
        this.runs.unshift(
          makeRun(localAt(now, 0, 1, 30) < now ? localAt(now, 0, 1, 30) : now - 2 * HOUR, "failed", {
            error: { kind: "config", message: "encryption key export not confirmed: run `sudo nestegg key confirm-exported`" },
          }),
        );
        this.reachable = null;
        break;
      case "credentials_unreadable":
        // Restored onto a new mainboard: the TPM-sealed secrets no longer decrypt.
        this.history(now, 13, 1, { skip: [10], pbsNight: 7 });
        this.runs.unshift(
          makeRun(localAt(now, 0, 1, 30) < now ? localAt(now, 0, 1, 30) : now - 2 * HOUR, "failed", {
            error: {
              kind: "credentials_unreadable",
              message:
                "cannot decrypt /etc/nestegg/credentials/encryption-key.cred (Failed to unseal secret using TPM2: State not recoverable); it is bound to this computer's TPM and host key. Store it again with: sudo nestegg key import <exported key file>",
            },
          }),
        );
        break;
      default:
        break;
    }
    this.runs = this.runs.filter((r) => new Date(r.started_at).getTime() < now);
    // Like the daemon: only runs that got to the server check (succeeded or unreachable) update it.
    const checked = this.runs.find((r) => r.state === "succeeded" || r.state === "pbs_unreachable");
    if (checked && this.reachable !== null) this.checkedMs = new Date(checked.started_at).getTime();
    this.rebuildSnapshots(now);
  }

  private rebuildSnapshots(now: number) {
    const fromRuns = this.runs
      .filter((r) => r.state === "succeeded")
      .map((r) => ({ ms: new Date(r.started_at).getTime(), size: 412 * GB }));
    const oldest = fromRuns.length ? Math.min(...fromRuns.map((s) => s.ms)) : null;
    const older: { ms: number; size: number }[] = [];
    if (oldest !== null) {
      for (let w = 1; w <= 3; w++) older.push({ ms: oldest - w * 7 * DAY, size: 405 * GB });
      for (let m = 1; m <= 6; m++) older.push({ ms: oldest - (21 + m * 30) * DAY, size: (398 - m * 6) * GB });
    }
    this.snapshots = [...fromRuns, ...older]
      .sort((a, b) => b.ms - a.ms)
      .map((s) => ({
        time: iso(s.ms),
        snapshot: `host/desktop/${iso(s.ms)}`,
        size_bytes: Math.round(s.size),
        archives: [
          ...ARCHIVES.map((a) => `${a.name}.didx`),
          ...this.extraSources.map((e) => `${e.archive}.didx`),
          "inventory.pxar.didx",
          "index.json.blob",
        ],
        encrypted: true,
        verified: s.ms < now - 3 * DAY ? "ok" : null,
        protected: false,
        comment: null,
      }));
  }

  private beginRun(startMs: number, outcome: ActiveRun["outcome"], trigger: Run["trigger"]) {
    const run = makeRun(startMs, "starting", { trigger });
    run.finished_at = null;
    run.duration_seconds = null;
    run.uploaded_bytes = null;
    run.snapshot = null;
    run.archives = [];
    this.active = { run, startedMs: startMs, outcome };
  }

  /** Advance the simulated run to the current time. */
  tick(now = Date.now()) {
    for (const job of this.restores.values()) {
      if (job.restore.state === "running" && now - job.startedMs > 4_000) {
        job.restore.state = "succeeded";
        job.restore.finished_at = iso(now);
      }
    }
    const a = this.active;
    if (!a) return;
    const el = now - a.startedMs;
    const run = a.run;
    if (a.outcome === "config") {
      if (el > 1_500) this.finish(now, makeRun(a.startedMs, "failed", { trigger: run.trigger, duration: 1, error: { kind: "config", message: "encryption key export not confirmed: run `sudo nestegg key confirm-exported`" } }));
      else run.state = "starting";
      return;
    }
    if (a.outcome === "pbs_unreachable") {
      if (el > 8_000) {
        this.reachable = false;
        this.checkedMs = now;
        this.finish(now, makeRun(a.startedMs, "pbs_unreachable", { trigger: run.trigger, duration: Math.round(el / 1000) }));
      } else run.state = el < 1_500 ? "starting" : "waking_pbs";
      return;
    }
    const total = 2.1 * GB;
    if (el < 1_500) run.state = "starting";
    else if (el < 5_000) run.state = "waking_pbs";
    else if (el < 8_000) run.state = "inventory";
    else if (el < 18_000) {
      run.state = "uploading";
      run.uploaded_bytes = Math.round(((el - 8_000) / 10_000) * total);
    } else if (el < 20_000) {
      run.state = "finishing";
      run.uploaded_bytes = total;
    } else {
      this.reachable = true;
      this.checkedMs = a.startedMs + 5_000;
      this.finish(now, makeRun(a.startedMs, "succeeded", { trigger: run.trigger, duration: Math.round(el / 1000), uploaded: total }));
    }
  }

  private finish(now: number, run: StoredRun) {
    run.id = this.active!.run.id;
    this.active = null;
    this.runs.unshift(run);
    if (run.state === "succeeded") this.rebuildSnapshots(now);
  }

  // ---------------------------------------------------------------- derived API objects

  private lastSuccess(): StoredRun | null {
    return this.runs.find((r) => r.state === "succeeded") ?? null;
  }

  private strip(r: StoredRun | null): Run | null {
    if (!r) return null;
    const { log_tail: _, ...rest } = r;
    return rest;
  }

  status(now = Date.now()): Status {
    const success = this.lastSuccess();
    const last = this.runs[0] ?? null;
    const overdueH = 36;
    let health: Status["health"];
    if (!this.configured) health = "unconfigured";
    else if (this.active) health = "running";
    else if (!success) health = "never";
    else if (last?.state === "failed") health = "failing";
    else if (now - new Date(success.finished_at ?? success.started_at).getTime() > overdueH * HOUR) health = "overdue";
    else health = "ok";
    const nextRun = (() => {
      if (!this.configured || this.active) return null;
      // The timer fires hourly; a run starts at the first full hour after it is due.
      const due = success ? new Date(success.started_at).getTime() + DAY : now;
      const next = new Date(Math.max(due, now));
      next.setMinutes(0, 0, 0);
      next.setHours(next.getHours() + 1);
      return iso(next.getTime());
    })();
    return {
      machine: { hostname: "desktop", backup_id: "desktop", os: "Linux Mint 22.2" },
      pbs: this.configured
        ? {
            server: "pbs.example.com:8007",
            datastore: "backups",
            namespace: "desktops",
            reachable: this.reachable,
            checked_at: this.checkedMs ? iso(this.checkedMs) : null,
            wake_on_lan: true,
          }
        : { server: "", datastore: "", namespace: null, reachable: null, checked_at: null, wake_on_lan: false },
      schedule: {
        enabled: this.configured && !this.faults.has("schedule_off"),
        calendar: "every 24 h (checked hourly)",
        next_run: this.faults.has("schedule_off") ? null : nextRun,
      },
      current_run: this.strip(this.active?.run ?? null),
      last_run: this.strip(last),
      last_success: this.strip(success),
      health,
      overdue_after_hours: overdueH,
      encryption: {
        enabled: true,
        key_fingerprint: this.configured ? "<FINGERPRINT>" : null, // placeholder, see AGENTS.md
        key_exported: this.keyExported,
      },
    };
  }

  config(): ConfigView | null {
    if (!this.configured) return null;
    return {
      backup_id: "desktop",
      sources: [...ARCHIVES.map((a) => ({ archive: a.name, path: a.source })), ...this.extraSources],
      excludes: ["**/.cache", "**/node_modules", "**/.local/share/Trash"],
      schedule: "every 24 h",
      encryption: true,
      overdue_after_hours: 36,
    };
  }

  machines(now = Date.now()): MachinesResponse {
    const snaps = (times: number[]) => times.map((ms) => iso(ms));
    const nightly = (from: number, to: number, skip: number[] = [], h = 1, m = 50) => {
      const out: number[] = [];
      for (let d = to; d <= from; d++) if (!skip.includes(d)) out.push(localAt(now, d, h, m + (d % 7)));
      return out.filter((ms) => ms < now);
    };
    const media = nightly(13, 1, [4, 7]);
    const laptop = nightly(13, 9, [12]);
    const workshop = nightly(13, 0, [7]);
    const machines: Machine[] = [
      {
        backup_id: "media",
        hostname: "media",
        os: "Linux Mint 22.2",
        last_snapshot: iso(media[0]!),
        recent_snapshots: snaps(media),
        size_bytes: 1_100 * GB,
        report: {
          reported_at: iso(localAt(now, 0, 1, 36)),
          result: "failed",
          run_started_at: iso(localAt(now, 0, 1, 31) < now ? localAt(now, 0, 1, 31) : localAt(now, 1, 23, 0)),
          error: { kind: "source_missing", message: "source /mnt/fotos: not a mount point (disk not mounted)" },
          key_exported: false,
          sources: ["/home", "/etc", "/root", "/usr/local", "/mnt/fotos"],
          overdue_after_hours: 36,
          nestegg_version: "0.3.0",
        },
      },
      {
        backup_id: "laptop",
        hostname: "laptop",
        os: "Ubuntu 24.04 LTS",
        last_snapshot: iso(laptop[0]!),
        recent_snapshots: snaps(laptop),
        size_bytes: 188 * GB,
        report: {
          reported_at: laptop.length ? iso(laptop[0]! + 15 * 60_000) : iso(now),
          result: "succeeded",
          key_exported: true,
          sources: ["/home", "/etc", "/root", "/usr/local"],
          overdue_after_hours: 36,
          nestegg_version: "0.3.0",
        },
      },
      {
        backup_id: "workshop",
        hostname: "workshop",
        os: "Ubuntu 24.04 LTS",
        last_snapshot: iso(workshop[0]!),
        recent_snapshots: snaps(workshop),
        size_bytes: 96 * GB,
        report: {
          reported_at: iso(workshop[0]! + 12 * 60_000),
          result: "succeeded",
          key_exported: true,
          sources: ["/home", "/etc", "/root", "/usr/local"],
          overdue_after_hours: 36,
          nestegg_version: "0.3.0",
        },
      },
      // This PC is part of the PBS listing too; the UI filters it out.
      { backup_id: "desktop", hostname: "desktop", last_snapshot: this.snapshots[0]?.time ?? null },
    ];
    return { machines, read_at: iso(Math.min(now, localAt(now, 0, 7, 20) < now ? localAt(now, 0, 7, 20) : now - 10 * 60_000)) };
  }

  // ---------------------------------------------------------------- request handling

  handle(method: string, path: string, query: URLSearchParams, body: unknown, now = Date.now()): Reply {
    if (this.scenario === "daemon_down" || this.faults.has("down")) {
      return {
        status: 502,
        body: { code: "internal", message: "cannot connect to daemon: No such file or directory (os error 2) (is nestegg.service running?)" },
      };
    }
    if (this.scenario === "unauthorized") {
      return { status: 401, body: "Not authorized. Open the address printed by `nestegg ui` in your terminal.\n" };
    }
    this.tick(now);
    const err = (status: number, code: string, message: string): Reply => ({ status, body: { code, message } });
    const notConfigured = () => err(500, "internal", "cannot read /etc/nestegg/config.toml: No such file or directory (os error 2)");
    const today = this.flavor === "today";

    const route = `${method} ${path}`;
    let m: RegExpExecArray | null;
    if (route === "GET /health") return { status: 200, body: { version: "0.1.0", api_version: "1" } };
    if (route === "GET /status") {
      const st = this.status(now);
      if (this.faults.has("no_run_details")) {
        delete st.last_run;
        delete st.last_success;
      }
      return { status: 200, body: st };
    }
    if (route === "GET /runs" && this.faults.has("runs")) return err(500, "internal", "cannot read /var/lib/nestegg/runs: Permission denied (os error 13)");
    if (route === "GET /config" && this.faults.has("config")) return err(500, "internal", "cannot parse /etc/nestegg/config.toml: expected `=`");
    if (route === "GET /runs") {
      const limit = Math.min(500, Math.max(1, Number(query.get("limit") ?? 50) || 50));
      const list = [...(this.active ? [this.active.run] : []), ...this.runs].slice(0, limit).map((r) => this.strip(r));
      return { status: 200, body: { runs: list } };
    }
    if (route === "POST /runs") {
      if (this.active) return err(409, "run_in_progress", "a backup run is already in progress");
      const outcome = !this.configured || !this.keyExported ? "config" : this.scenario === "pbs_unreachable" || this.scenario === "overdue" ? "pbs_unreachable" : "succeeded";
      this.beginRun(now, outcome, "manual");
      return { status: 202, body: this.strip(this.active!.run) };
    }
    if (method === "GET" && (m = /^\/runs\/([^/]+)$/.exec(path))) {
      const id = decodeURIComponent(m[1]!);
      const run = this.active?.run.id === id ? this.active.run : this.runs.find((r) => r.id === id);
      if (!run) return err(404, "not_found", "unknown run");
      return { status: 200, body: run satisfies RunDetail };
    }
    if (route === "GET /snapshots") {
      if (!this.configured) return notConfigured();
      if (this.reachable === false) return err(503, "pbs_unreachable", "PBS is not reachable");
      return { status: 200, body: { snapshots: this.snapshots } };
    }
    if (method === "GET" && (m = /^\/snapshots\/([^/]+)\/browse$/.exec(path))) {
      if (today) return err(501, "not_implemented", "planned for v0.2");
      if (this.reachable === false) return err(503, "pbs_unreachable", "PBS is not reachable");
      const time = decodeURIComponent(m[1]!);
      if (!this.snapshots.some((s) => s.time === time)) return err(404, "not_found", "unknown snapshot");
      const entries = browseTree(query.get("archive") ?? "", query.get("path") ?? "/");
      if (!entries) return err(404, "not_found", "no such directory");
      return { status: 200, body: { entries } };
    }
    if (route === "POST /restores") {
      if (today) return err(501, "not_implemented", "planned for v0.2");
      const req = body as Partial<RestoreRequest> | null;
      if (!req || !req.snapshot_time || !req.archive || !Array.isArray(req.paths) || !req.target_dir)
        return err(400, "bad_request", "snapshot_time, archive, paths and target_dir are required");
      if (!req.target_dir.startsWith("/")) return err(400, "bad_request", "target_dir must be an absolute path");
      if (EXISTING_TARGETS.includes(req.target_dir.replace(/\/$/, "")))
        return err(409, "target_exists", `target directory ${req.target_dir} already exists`);
      const id = `restore-${this.restores.size + 1}`;
      const restore: Restore = { id, state: "running", target_dir: req.target_dir, started_at: iso(now), finished_at: null, error: null };
      this.restores.set(id, { restore, startedMs: now });
      return { status: 202, body: restore };
    }
    if (method === "GET" && (m = /^\/restores\/([^/]+)$/.exec(path))) {
      const job = this.restores.get(decodeURIComponent(m[1]!));
      if (!job) return err(404, "not_found", "unknown restore");
      return { status: 200, body: job.restore };
    }
    if (route === "GET /config") {
      const c = this.config();
      return c ? { status: 200, body: c } : notConfigured();
    }
    // Proposed endpoints (API-REQUESTS.md).
    if (route === "GET /machines" && !today) return { status: 200, body: this.machines(now) };
    if (route === "GET /session" && !today) return { status: 200, body: { user: "alice", home: "/home/alice" } };
    return err(404, "not_found", "unknown endpoint");
  }
}
