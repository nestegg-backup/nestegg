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
  CheckStatus,
  PbsCheckItem,
  PbsCheckResult,
  PbsInfo,
  Restore,
  RestoreRequest,
  Run,
  RunDetail,
  SettingsChange,
  SettingsLimits,
  Snapshot,
  Status,
  Target,
  TargetStatus,
} from "../src/lib/types.ts";
import { DEFAULT_LIMITS, OVERDUE_BASE_HOURS, OVERDUE_INTERVAL_EXTRA_HOURS, SETTING_KEYS, overdueCap, settingsProblem } from "../src/lib/settings.ts";
import { malformedProblem, wholeFolderProblem } from "./folderRule.ts";

/** An item of the connection check, as `check::run` of the daemon builds it. */
function item(name: string, status: CheckStatus, detail: string): PbsCheckItem {
  return { name, ok: status === "passed", status, detail };
}

export const SCENARIOS = [
  "ok",
  "running",
  "overdue",
  "failing",
  "never",
  "reinstalled",
  "unconfigured",
  "pbs_unreachable",
  "key_not_exported",
  "credentials_unreadable",
  "incomplete",
  "verification_failed",
  "damaged",
  "damaged_all",
  "no_credentials",
  "no_key",
  "no_timer",
  "cancelled",
  "unknown_run",
  "unknown_never",
  "daemon_down",
  "unauthorized",
  // Backup stick (decisions 0023 and 0026). `stick_*` without a server, `both*` with one.
  "stick_only",
  "stick_absent",
  "stick_full",
  "stick_running",
  "both",
  "both_absent",
  "stick_damaged",
  "stick_verify_failed",
  "stick_key_mismatch",
  "stick_unsupported",
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
 * - `schedule_off`: the timer is disabled (automatic backups off; `PATCH /config` can switch it on)
 * - `no_run_details`: `/status` omits last_run and last_success (allowed by the contract)
 * - `no_wake`: the configuration has no `[wake]` section (Wake-on-LAN cannot be switched on)
 * - `timer_fails`: switching automatic backups fails like a missing systemd (500, nothing changes)
 * - `unverified`: runs carry no `verified` flag (records of older versions)
 * - `cancel_late`: the backup has just ended when the cancel request arrives: 409 `cannot_cancel`
 *   (the daemon answers 404 for a run id it does not know and 409 for a run that exists but is over
 *   or still starting; the `daemon=today` flavour answers 404 for all of it, like older daemons)
 * - `check_slow`: `POST /pbs/check` takes 6 s (like a PBS that has to be woken up)
 * - `check_bad`: the connection check finds a problem (login refused)
 * - `check_down`: the backup server is switched off (it does not wake): the connection check answers
 *   200 like the daemon. Without Wake-on-LAN "PBS reachable" fails; with it the item is `not_checked`
 *   (nobody asked to wake it), or `failed` after a wake attempt (`wake=true`). The login and
 *   datastore items are `not_checked` in every case
 * - `no_usage`: `GET /pbs` does not know the datastore usage
 * - `no_encryption`: encryption is switched off
 * - `unknown_stored`: `credentials_stored` and `key_present` are null (the daemon cannot tell)
 * - `long_overdue`: an administrator set "overdue after" to 744 hours in the file: above the API's
 *   limit of 168, so the settings page can only lower it (`limits` reports 744 as the maximum)
 * - `stick_password`: the stick's key copy is due for its password re-check (`password_reminder`)
 * - `stick_pulled`: the stick is pulled out (also at runtime: `POST /__mock/fault?stick_pulled=1|0`);
 *   stick requests answer 503 `stick_unavailable` and a running restore from it fails
 * - `stick_busy`: opening the stick fails although it is plugged in (503 `stick_unavailable`)
 * - `stick_no_progress`: like a daemon before API 0.6.0, a stick restore reports its bytes only at the end
 */
export const FAULTS = [
  "down",
  "runs",
  "config",
  "schedule_off",
  "no_run_details",
  "no_wake",
  "timer_fails",
  "unverified",
  "cancel_late",
  "check_slow",
  "check_bad",
  "check_down",
  "no_usage",
  "no_encryption",
  "unknown_stored",
  "long_overdue",
  "stick_password",
  "stick_pulled",
  "stick_busy",
  "stick_multiple",
  "stick_no_progress",
] as const;
export type Fault = (typeof FAULTS)[number];

export interface Reply {
  status: number;
  body: unknown;
  /** Answer later than the usual short latency (ms). */
  delayMs?: number;
}

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const GB = 1_000_000_000;

/** The documentation UUID of docs/stick.md; never a real medium. */
export const SECOND_STICK_UUID = "fedcba98-7654-3210-fedc-ba9876543210";
export const STICK_UUID = "01234567-89ab-cdef-0123-456789abcdef";
/** Only the first stick has a name (`[stick.names]`); the second shows the fallback. */
export const STICK_NAMES: Record<string, string> = { [STICK_UUID]: "Blue stick" };
/** The client's "(compressed …)" is about this share of the new data in the mock. */
const COMPRESSED = 0.38;

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
  /** `stick_full`, `absent`: stick runs only. */
  outcome: "succeeded" | "pbs_unreachable" | "config" | "stick_full" | "absent";
  /** "Back up now" with both targets: the server's run follows the stick's. */
  then?: "pbs";
  /** Message of a `config` failure. */
  configMessage?: string;
  /** When the user asked to cancel (ms); the run ends as `cancelled` shortly after. */
  cancelAtMs?: number;
}

interface RestoreJob {
  restore: Restore;
  startedMs: number;
  totalBytes: number;
  /** From the stick: it fails when the stick is pulled out. */
  stick?: boolean;
  /** Whole archives are restored: the daemon knows a total. */
  wholeArchives?: boolean;
}

const fmtClock = (ms: number) => new Date(ms).toTimeString().slice(0, 8);

/** The stick as the mock knows it. */
interface StickWorld {
  plugged: boolean;
  availableBytes: number;
  overdueHours: number;
  runs: StoredRun[];
  fullVerifiedMs: number | null;
  damagedPaths: string[];
}

function stickLogFor(run: Run, startMs: number): string[] {
  const t = (offsetS: number) => fmtClock(startMs + offsetS * 1000);
  const end = run.duration_seconds ?? 600;
  const head = `${t(0)}  start: trigger=${run.trigger} target=stick backup-id=desktop`;
  const mounted = [
    `${t(1)}  stick ${STICK_UUID}: e2fsck -f -n clean; mounted nodev,nosuid,noexec,relatime`,
    `${t(3)}  local PBS started from /var/lib/nestegg/stick/pbs.squashfs (loopback only)`,
  ];
  switch (run.state) {
    case "stick_not_plugged_in":
      return [head, `${t(0)}  no registered stick is plugged in`, `${t(0)}  result: stick_not_plugged_in`];
    case "succeeded":
      return [
        head,
        ...mounted,
        `${t(6)}  proxmox-backup-client backup home.pxar:/home etc.pxar:/etc root.pxar:/root usr-local.pxar:/usr/local inventory.pxar`,
        `${t(end - 40)}  uploaded ${((run.uploaded_bytes ?? 0) / GB).toFixed(2)} GB; read back and compared`,
        `${t(end - 20)}  PBS verification of the new snapshot: ok`,
        `${t(end - 10)}  prune keep-last=3 keep-daily=7 keep-weekly=4 keep-monthly=6; garbage collection done`,
        `${t(end)}  local PBS stopped, file system flushed and unmounted: safe to unplug`,
        `${t(end)}  result: succeeded (${run.snapshot})`,
      ];
    case "failed":
      return [
        head,
        ...(run.error?.kind === "stick_unsupported" || run.error?.kind === "filesystem_damaged" ? [] : mounted),
        `${t(end - 2)}  ${run.error?.message ?? "error"}`,
        `${t(end)}  local PBS stopped and stick unmounted`,
        `${t(end)}  result: failed (${run.error?.kind ?? "internal"})`,
      ];
    case "cancelled":
      return [head, ...mounted, `${t(end - 1)}  cancelled by the user`, `${t(end)}  result: cancelled`];
    default:
      return [head];
  }
}

function logFor(run: Run, startMs: number): string[] {
  if (run.target === "stick") return stickLogFor(run, startMs);
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
    case "cancelled":
      return [head, `${t(1)}  pbs.example.com:8007 reachable`, `${t(9)}  cancelled by the user`, `${t(run.duration_seconds ?? 9)}  result: cancelled`];
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
  opts: {
    trigger?: Run["trigger"];
    duration?: number;
    uploaded?: number;
    error?: Run["error"];
    warnings?: string[];
    incomplete?: string[];
    verified?: boolean;
    /** Only set when given: PBS records of older versions have no target. */
    target?: Target;
    /** A record of an older version: no `transferred_bytes`. */
    legacy?: boolean;
  } = {},
): StoredRun {
  const d = new Date(startMs);
  const stamp = d.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
  const trigger = opts.trigger ?? "timer";
  const duration =
    opts.duration ?? (state === "stick_not_plugged_in" ? 1 : state === "pbs_unreachable" ? 300 : state === "failed" ? 66 : 600);
  const run: Run = {
    ...(opts.target ? { target: opts.target } : {}),
    ...(opts.target === "stick" && state !== "stick_not_plugged_in" ? { stick_uuid: STICK_UUID } : {}),
    id: `${stamp}-${trigger}${opts.target === "stick" ? "-stick" : ""}`,
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
    ...(state === "succeeded" && !opts.legacy ? { transferred_bytes: Math.round((opts.uploaded ?? GB) * COMPRESSED) } : {}),
    // Like the daemon: every reason for "incomplete" is also a warning.
    warnings: [...(opts.warnings ?? []), ...(opts.incomplete ?? [])],
    ...(opts.incomplete?.length ? { incomplete: opts.incomplete } : {}),
    ...(state === "succeeded" && opts.verified !== undefined ? { verified: opts.verified } : {}),
    expected_bytes: state === "succeeded" ? 412 * GB : null,
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
  /** Settings the web UI can change (`PATCH /config`). */
  intervalHours = 24;
  overdueHours = 36;
  /** `PATCH /config` has set the threshold: the `long_overdue` fault no longer applies. */
  private overdueSet = false;
  excludes = ["**/.cache", "**/node_modules", "**/.local/share/Trash"];
  wakeEnabled = true;
  /** null: follows the `schedule_off` fault; set by `PATCH /config`. */
  timerEnabled: boolean | null = null;
  /** Snapshots made before the local state was lost (`reinstalled`): on the server, unknown to the daemon. */
  inherited: Snapshot[] = [];
  /** Snapshot paths the server's own verification marked as failed (`damaged`). */
  damagedPaths: string[] = [];
  /** `damaged_all`: every snapshot is damaged (also the older ones made up in `rebuildSnapshots`). */
  allDamaged = false;
  /** false: encryption is on, but no key exists yet (`no_key`, `unconfigured`). */
  keyCreated = true;
  /** false: the token secret is not stored yet (`no_credentials`, `unconfigured`). */
  credentialsStored = true;
  /** The configuration has a `[pbs]` section (false: the stick is the only target). */
  pbsTarget = true;
  /** The configuration has a `[stick]` section. */
  stick: StickWorld | null = null;
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
        // Backups from the last ten days were read back after the upload; older records have no such flag.
        verified: d <= 9 ? true : undefined,
        // Records older than eleven days come from a version without the compressed amount.
        legacy: d > 11,
      });
      this.runs.unshift(run);
    }
  }

  /** Stick backups on the given days (days ago), in the evening, started by plugging the stick in. */
  private stickHistory(now: number, days: number[], last?: number) {
    const r = rng(7);
    const runs: StoredRun[] = [];
    for (const d of days) {
      const start = d === 0 && last !== undefined ? last : localAt(now, d, 19, 10 + Math.floor(r() * 30));
      if (start > now) continue;
      runs.unshift(
        makeRun(start, "succeeded", {
          target: "stick",
          trigger: "plug",
          duration: 420 + Math.floor(r() * 300),
          uploaded: Math.round((0.6 + r() * 2.4) * GB),
          verified: true,
        }),
      );
    }
    return runs;
  }

  private makeStick(now: number, plugged: boolean, days: number[], opts: { last?: number; availableBytes?: number } = {}): StickWorld {
    return {
      plugged,
      availableBytes: opts.availableBytes ?? 212 * GB,
      overdueHours: 168,
      runs: this.stickHistory(now, days, opts.last),
      fullVerifiedMs: localAt(now, 9, 19, 40),
      damagedPaths: [],
    };
  }

  /** A failed stick run today (or two hours ago, before 19:00). */
  private stickFailure(now: number, kind: NonNullable<Run["error"]>["kind"], message: string) {
    const at = localAt(now, 0, 19, 12) < now ? localAt(now, 0, 19, 12) : now - 2 * HOUR;
    this.stick!.runs.unshift(makeRun(at, "failed", { target: "stick", trigger: "plug", duration: 95, error: { kind, message } }));
  }

  private build(now: number) {
    switch (this.scenario) {
      case "stick_only":
        this.pbsTarget = false;
        this.reachable = null;
        this.stick = this.makeStick(now, true, [13, 11, 8, 6, 4, 2, 0], { last: now - 70 * 60_000, availableBytes: 312 * GB });
        break;
      case "stick_absent":
        // Last plugged in nine days ago: overdue after seven. The hourly timer looked for it today.
        this.pbsTarget = false;
        this.reachable = null;
        this.stick = this.makeStick(now, false, [13, 9]);
        for (const h of [3, 2, 1]) this.stick.runs.unshift(makeRun(now - h * HOUR, "stick_not_plugged_in", { target: "stick", error: { kind: "stick_not_plugged_in", message: "stick not plugged in" } }));
        break;
      case "stick_full":
        this.pbsTarget = false;
        this.reachable = null;
        this.stick = this.makeStick(now, true, [13, 11, 8, 6, 4, 2], { availableBytes: 180 * 1_000_000 });
        this.stickFailure(now, "stick_full", "stick full: insufficient space available to the PBS user; retention policy was kept");
        break;
      case "both":
      case "both_absent":
      case "stick_running":
      case "stick_damaged":
      case "stick_verify_failed":
      case "stick_key_mismatch":
      case "stick_unsupported": {
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        for (const r of this.runs) r.target = "pbs";
        const absent = this.scenario === "both_absent";
        this.stick = this.makeStick(now, !absent, absent ? [12, 8, 4] : [12, 9, 5, 2]);
        if (absent) {
          for (const h of [2, 1]) this.stick.runs.unshift(makeRun(now - h * HOUR - 20 * 60_000, "stick_not_plugged_in", { target: "stick", error: { kind: "stick_not_plugged_in", message: "stick not plugged in" } }));
        }
        if (this.scenario === "stick_running") this.beginRun(now - 9_000, "succeeded", "plug", "stick");
        if (this.scenario === "stick_damaged")
          this.stickFailure(now, "filesystem_damaged", "e2fsck -f -n on the stick's data partition: Free blocks count wrong for group #0 (expected 0, counted 512); repair the unmounted file system offline");
        if (this.scenario === "stick_verify_failed") {
          this.stickFailure(now, "verification_failed", "PBS verification of the new snapshot failed: chunk 3f9a… is corrupt");
          this.stick.damagedPaths = [this.stick.runs.find((r) => r.state === "succeeded")!.snapshot!];
        }
        if (this.scenario === "stick_key_mismatch")
          this.stickFailure(now, "key_mismatch", "the newest snapshot of host/desktop on this target was made with another encryption key; nothing was written or pruned");
        if (this.scenario === "stick_unsupported")
          this.stickFailure(now, "stick_unsupported", "kernel IPv6 socket support is missing (Address family not supported by protocol); the local PBS needs it, also for loopback over IPv4. Host settings were not changed.");
        break;
      }
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
      case "reinstalled":
        // Two weeks of backups, then the system was reinstalled: see below.
        this.history(now, 13, 1, { skip: [10], pbsNight: 7 });
        break;
      case "unconfigured":
        this.configured = false;
        this.keyExported = false;
        this.keyCreated = false;
        this.credentialsStored = false;
        this.reachable = null;
        break;
      case "no_credentials":
        // `nestegg init` has run; the token secret is not stored yet, and no key either.
        this.credentialsStored = false;
        this.keyExported = false;
        this.keyCreated = false;
        this.reachable = null;
        break;
      case "no_key":
        // Set up, but `nestegg key create` has not run yet.
        this.keyExported = false;
        this.keyCreated = false;
        this.reachable = null;
        break;
      case "no_timer":
        // Everything is ready except the timer: automatic backups are off, no backup yet.
        this.timerEnabled = false;
        this.reachable = null;
        break;
      case "incomplete":
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        // The last three backups left something out.
        for (const run of this.runs.filter((r) => r.state === "succeeded").slice(0, 3)) {
          const reasons = [
            "full system backup skipped: the root file system (zfs) cannot be recreated by the rescue stick; only the configured folders are backed up",
            ...(run === this.runs.find((r) => r.state === "succeeded") ? ["optional source /mnt/data does not exist, skipped"] : []),
          ];
          run.incomplete = reasons;
          run.warnings = [...(run.warnings ?? []), ...reasons];
          run.log_tail = logFor(run, new Date(run.started_at).getTime());
        }
        break;
      case "verification_failed":
        this.history(now, 13, 1, { skip: [10], pbsNight: 7 });
        this.runs.unshift(
          makeRun(localAt(now, 0, 1, 31) < now ? localAt(now, 0, 1, 31) : now - 2 * HOUR, "failed", {
            duration: 840,
            error: {
              kind: "verification_failed",
              message: "snapshot host/desktop/2026-11-09T00:31:08Z was uploaded, but reading it back failed: archive home.pxar.didx: chunk 3f9a… is missing on the server",
            },
          }),
        );
        break;
      case "damaged": {
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        const done = this.runs.filter((r) => r.state === "succeeded");
        this.damagedPaths = [done[4], done[8]].filter((r): r is StoredRun => !!r && !!r.snapshot).map((r) => r.snapshot!);
        break;
      }
      case "damaged_all":
        // Every snapshot the server holds is damaged: the restore picker has nothing better to offer.
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        this.allDamaged = true;
        break;
      case "cancelled":
        this.history(now, 13, 0, { skip: [10], pbsNight: 7 });
        this.runs.unshift(
          makeRun(now - 3 * HOUR, "cancelled", {
            trigger: "manual",
            duration: 140,
            error: { kind: "cancelled", message: "cancelled by the user" },
          }),
        );
        break;
      case "unknown_run": {
        // Records written by a newer version: the newest run has a state this version does not know
        // (the daemon reports health `failing` then), an older success has a trigger it does not know,
        // and an older failure has an error kind it does not know.
        this.history(now, 13, 1, { skip: [10, 5], pbsNight: 7 });
        const done = this.runs.filter((r) => r.state === "succeeded");
        if (done[1]) done[1].trigger = "unknown";
        // The failure is on a day of its own (the unknown run is today), so both show on the strip.
        const failedAt = localAt(now, 5, 1, 40);
        const failed = makeRun(failedAt, "failed", {
          error: { kind: "unknown", message: "quota exceeded on the server (error kind added in a later version)" },
        });
        this.runs.splice(this.runs.findIndex((r) => new Date(r.started_at).getTime() < failedAt), 0, failed);
        this.runs.unshift(makeRun(now - 2 * HOUR, "unknown", { trigger: "unknown" }));
        break;
      }
      case "unknown_never":
        // No backup yet, and the one attempt that exists was written by a newer version.
        this.runs.unshift(makeRun(now - 2 * HOUR, "unknown", { trigger: "unknown" }));
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
    if (this.allDamaged) this.damagedPaths = this.snapshots.map((x) => x.snapshot);
    if (this.scenario === "reinstalled") {
      // Like a fresh install with the same backup id: /var/lib/nestegg is empty
      // (no runs, no last_success, server never checked), the PBS keeps every snapshot.
      this.inherited = this.snapshots;
      this.runs = [];
      this.checkedMs = null;
      this.reachable = null;
    }
  }

  private rebuildSnapshots(now: number) {
    const fromRuns = this.runs
      .filter((r) => r.state === "succeeded")
      .map((r) => ({ ms: new Date(r.started_at).getTime(), size: 412 * GB }));
    const oldest = fromRuns.length ? Math.min(...fromRuns.map((s) => s.ms)) : null;
    const older: { ms: number; size: number }[] = [];
    if (oldest !== null && !this.inherited.length) {
      for (let w = 1; w <= 3; w++) older.push({ ms: oldest - w * 7 * DAY, size: 405 * GB });
      for (let m = 1; m <= 6; m++) older.push({ ms: oldest - (21 + m * 30) * DAY, size: (398 - m * 6) * GB });
    }
    const made: Snapshot[] = [...fromRuns, ...older].map((s) => ({
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
        verified: this.allDamaged || this.damagedPaths.includes(`host/desktop/${iso(s.ms)}`) ? "failed" : s.ms < now - 3 * DAY ? "ok" : null,
        protected: false,
        comment: null,
      }));
    this.snapshots = [...made, ...this.inherited].sort((a, b) => b.time.localeCompare(a.time));
  }

  private beginRun(startMs: number, outcome: ActiveRun["outcome"], trigger: Run["trigger"], target?: Target, then?: "pbs") {
    const run = makeRun(startMs, "starting", { trigger, target });
    run.finished_at = null;
    run.duration_seconds = null;
    run.uploaded_bytes = null;
    run.snapshot = null;
    run.archives = [];
    run.warnings = [];
    if (target === "stick" && outcome === "absent") delete run.stick_uuid;
    this.active = { run, startedMs: startMs, outcome, then };
  }

  /** How a run to the server ends in this scenario. */
  private pbsOutcome(): ActiveRun["outcome"] {
    return this.scenario === "pbs_unreachable" || this.scenario === "overdue" ? "pbs_unreachable" : "succeeded";
  }

  /** How a run to the stick ends in this scenario. */
  private stickOutcome(): ActiveRun["outcome"] {
    if (!this.stickIn()) return "absent";
    return this.scenario === "stick_full" ? "stick_full" : "succeeded";
  }

  /**
   * Start a run as `POST /runs` and `POST /targets/{target}/runs` do; null when it starts, the
   * daemon's refusal otherwise. `all` backs up to the stick first, then to the server.
   */
  private startRun(target: Target | "all", now: number): Reply | null {
    const err = (status: number, code: string, message: string): Reply => ({ status, body: { code, message } });
    if (this.active) return err(409, "run_in_progress", "a backup run is already in progress");
    const wantStick = target !== "pbs" && !!this.stick;
    const wantPbs = target !== "stick" && this.pbsTarget;
    if ((target === "stick" && !this.stick) || (target === "pbs" && !this.pbsTarget) || (!wantStick && !wantPbs))
      return err(400, "invalid_target", "requested target is not configured");
    const needKey = this.encryptionOn() && !this.keyExported;
    const first: Target = wantStick ? "stick" : "pbs";
    const blocked = !this.configured || (this.pbsTarget && !this.credentialsStored) || needKey;
    if (blocked) {
      this.beginRun(now, "config", "manual", this.stick ? first : undefined);
      if (this.pbsTarget && !this.credentialsStored) {
        this.active!.configMessage = "password / token secret: cannot read /etc/nestegg/credentials/password.cred: No such file or directory (os error 2); store it with `sudo nestegg credentials set-password`";
      }
      return null;
    }
    if (first === "stick") this.beginRun(now, this.stickOutcome(), "manual", "stick", wantPbs ? "pbs" : undefined);
    else this.beginRun(now, this.pbsOutcome(), "manual", this.stick ? "pbs" : undefined);
    return null;
  }

  /** A registered stick is plugged in right now. */
  stickIn(): boolean {
    return !!this.stick?.plugged && !this.faults.has("stick_pulled");
  }

  /** Snapshots on the stick, made by its successful runs (`GET /snapshots?target=stick`). */
  stickSnapshots(): Snapshot[] {
    return (this.stick?.runs ?? [])
      .filter((r) => r.state === "succeeded" && r.snapshot)
      .map((r) => ({
        time: r.started_at,
        snapshot: r.snapshot!,
        size_bytes: 412 * GB,
        archives: [...ARCHIVES.map((a) => `${a.name}.didx`), "inventory.pxar.didx", "index.json.blob"],
        encrypted: true,
        verified: this.stick!.damagedPaths.includes(r.snapshot!) ? "failed" : "ok",
        protected: false,
        comment: null,
      }));
  }

  /** Advance the simulated run to the current time. */
  tick(now = Date.now()) {
    for (const job of this.restores.values()) {
      if (job.restore.state !== "running") continue;
      const el = now - job.startedMs;
      if (job.stick) {
        // Like the stick worker: it opens the stick first (about 2 s), then reports the bytes
        // copied so far once a second; an older daemon (fault `stick_no_progress`) only at the end.
        if (!this.stickIn()) {
          job.restore.state = "failed";
          job.restore.finished_at = iso(now);
          job.restore.error = "reading the stick failed: the device disappeared (stick removed?); nothing was moved into the target folder";
        } else if (el > 8_000) {
          job.restore.state = "succeeded";
          job.restore.finished_at = iso(now);
          job.restore.restored_bytes = Math.round(job.totalBytes);
        } else if (el > 2_000 && !this.faults.has("stick_no_progress")) {
          job.restore.restored_bytes = Math.round(Math.floor((el - 2_000) / 1_000) / 6 * job.totalBytes);
          job.restore.total_bytes = job.wholeArchives ? Math.round(job.totalBytes) : null;
        }
        continue;
      }
      job.restore.restored_bytes = Math.round(Math.min(1, el / 4_000) * job.totalBytes);
      if (el > 4_000) {
        job.restore.state = "succeeded";
        job.restore.finished_at = iso(now);
      }
    }
    const a = this.active;
    if (!a) return;
    const el = now - a.startedMs;
    const run = a.run;
    const target = run.target;
    if (a.cancelAtMs !== undefined && now - a.cancelAtMs > 1_200) {
      this.finish(now, makeRun(a.startedMs, "cancelled", { target, trigger: run.trigger, duration: Math.round(el / 1000), error: { kind: "cancelled", message: "cancelled by the user" } }));
      return;
    }
    if (a.outcome === "config") {
      if (el > 1_500) this.finish(now, makeRun(a.startedMs, "failed", { target, trigger: run.trigger, duration: 1, error: { kind: "config", message: a.configMessage ?? "encryption key export not confirmed: run `sudo nestegg key confirm-exported`" } }));
      else run.state = "starting";
      return;
    }
    if (target === "stick") {
      const stick = this.stick!;
      if (a.outcome === "absent") {
        if (el > 1_500) this.finish(now, makeRun(a.startedMs, "stick_not_plugged_in", { target, trigger: run.trigger, error: { kind: "stick_not_plugged_in", message: "stick not plugged in" } }));
        else run.state = "starting";
        return;
      }
      if (a.outcome === "stick_full") {
        if (el > 3_000) this.finish(now, makeRun(a.startedMs, "failed", { target, trigger: run.trigger, duration: 3, error: { kind: "stick_full", message: "stick full: insufficient space available to the PBS user; retention policy was kept" } }));
        else run.state = "starting";
        return;
      }
      const total = 1.6 * GB;
      if (el < 2_000) run.state = "starting";
      else if (el < 5_000) run.state = "inventory";
      else if (el < 14_000) {
        run.state = "uploading";
        const progress = (el - 5_000) / 9_000;
        run.uploaded_bytes = Math.round(progress * total);
        run.transferred_bytes = Math.round(progress * total * COMPRESSED);
        run.expected_bytes = 412 * GB;
        run.processed_bytes = Math.round((Math.floor(progress * 5) / 5) * 412 * GB);
      } else if (el < 19_000) {
        run.state = "finishing";
        run.uploaded_bytes = total;
        run.transferred_bytes = Math.round(total * COMPRESSED);
      } else {
        stick.availableBytes = Math.max(0, stick.availableBytes - total);
        this.finish(now, makeRun(a.startedMs, "succeeded", { target, trigger: run.trigger, duration: Math.round(el / 1000), uploaded: total, verified: true }));
      }
      return;
    }
    if (a.outcome === "pbs_unreachable") {
      if (el > 8_000) {
        this.reachable = false;
        this.checkedMs = now;
        this.finish(now, makeRun(a.startedMs, "pbs_unreachable", { target, trigger: run.trigger, duration: Math.round(el / 1000) }));
      } else run.state = el < 1_500 ? "starting" : "waking_pbs";
      return;
    }
    const total = 2.1 * GB;
    if (el < 1_500) run.state = "starting";
    else if (el < 5_000) run.state = "waking_pbs";
    else if (el < 8_000) run.state = "inventory";
    else if (el < 18_000) {
      run.state = "uploading";
      const progress = (el - 8_000) / 10_000;
      run.uploaded_bytes = Math.round(progress * total);
      run.transferred_bytes = Math.round(progress * total * COMPRESSED);
      // Archive sizes are only known after each archive: the estimate moves in steps.
      run.expected_bytes = 412 * GB;
      run.processed_bytes = Math.round((Math.floor(progress * 5) / 5) * 412 * GB);
    } else if (el < 20_000) {
      run.state = "finishing";
      run.uploaded_bytes = total;
      run.transferred_bytes = Math.round(total * COMPRESSED);
    } else {
      this.reachable = true;
      this.checkedMs = a.startedMs + 5_000;
      this.finish(now, makeRun(a.startedMs, "succeeded", { target, trigger: run.trigger, duration: Math.round(el / 1000), uploaded: total, verified: true }));
    }
  }

  private finish(now: number, run: StoredRun) {
    const a = this.active!;
    run.id = a.run.id;
    this.active = null;
    if (run.target === "stick") {
      this.stick!.runs.unshift(run);
    } else {
      this.runs.unshift(run);
      if (run.state === "succeeded") this.rebuildSnapshots(now);
    }
    // "Back up now" with both targets: the server follows the stick, under the same run lock.
    if (a.then === "pbs" && run.state !== "cancelled") this.beginRun(now, this.pbsOutcome(), run.trigger, "pbs");
  }

  // ---------------------------------------------------------------- derived API objects

  /** Every finished run of both targets, newest first. */
  private allRuns(): StoredRun[] {
    return [...this.runs, ...(this.stick?.runs ?? [])].sort((a, b) => b.started_at.localeCompare(a.started_at));
  }

  private lastSuccess(): StoredRun | null {
    return this.runs.find((r) => r.state === "succeeded") ?? null;
  }

  private strip(r: StoredRun | null): Run | null {
    if (!r) return null;
    const { log_tail: _, ...rest } = r;
    if (this.faults.has("unverified")) delete rest.verified;
    return rest;
  }

  /** Health of one target from its own runs, like `state::health` of the daemon. */
  private healthOf(runs: StoredRun[], running: boolean, overdueH: number, now: number): Status["health"] {
    const success = runs.find((r) => r.state === "succeeded") ?? null;
    const last = runs[0] ?? null;
    if (!this.configured) return "unconfigured";
    if (running) return "running";
    if (!success) return "never";
    if (last?.state === "failed" || last?.state === "unknown") return "failing";
    if (now - new Date(success.finished_at ?? success.started_at).getTime() > overdueH * HOUR) return "overdue";
    if (success.incomplete?.length) return "incomplete";
    return "ok";
  }

  /** The timer fires hourly; a run starts at the first full hour after it is due. */
  private nextRunOf(runs: StoredRun[], now: number): string | null {
    if (!this.configured || this.active) return null;
    const success = runs.find((r) => r.state === "succeeded") ?? null;
    const due = success ? new Date(success.started_at).getTime() + this.intervalHours * HOUR : now;
    const next = new Date(Math.max(due, now));
    next.setMinutes(0, 0, 0);
    next.setHours(next.getHours() + 1);
    return iso(next.getTime());
  }

  /** `targets` of `/status`: only the configured ones (none before the setup). */
  private targets(now: number): Partial<Record<Target, TargetStatus>> {
    const out: Partial<Record<Target, TargetStatus>> = {};
    if (!this.configured) return out;
    const active = (t: Target) => (this.active && (this.active.run.target ?? "pbs") === t ? this.strip(this.active.run) : null);
    const failures = (runs: StoredRun[]) => {
      let n = 0;
      for (const r of runs) {
        if (r.state === "succeeded") break;
        if (r.state === "failed" || r.state === "unknown") n++;
      }
      return n;
    };
    if (this.pbsTarget) {
      const health = this.healthOf(this.runs, !!active("pbs"), this.overdue(), now);
      out.pbs = {
        health,
        state: health,
        last_run: this.strip(this.runs[0] ?? null),
        last_success: this.strip(this.lastSuccess()),
        current_run: active("pbs"),
        consecutive_failures: failures(this.runs),
        overdue_after_hours: this.overdue(),
        next_run: this.timerOn() ? this.nextRunOf(this.runs, now) : null,
        available_bytes: null,
        full_verified_at: null,
        server_verify_failed: [...this.damagedPaths],
        password_reminder: false,
        password_reminders: {},
        plugged_in: null,
      };
    }
    const st = this.stick;
    if (st) {
      const running = active("stick");
      const health = this.healthOf(st.runs, !!running, st.overdueHours, now);
      const last = st.runs[0] ?? null;
      const state = running
        ? health
        : last?.state === "failed" && last.error?.kind === "stick_full"
          ? "stick_full"
          : !this.stickIn()
            ? "stick_not_plugged_in"
            : health;
      out.stick = {
        health,
        state,
        last_run: this.strip(last),
        last_success: this.strip(st.runs.find((r) => r.state === "succeeded") ?? null),
        current_run: running,
        consecutive_failures: failures(st.runs),
        overdue_after_hours: st.overdueHours,
        next_run: this.timerOn() ? this.nextRunOf(st.runs, now) : null,
        available_bytes: st.runs.some((r) => r.state === "succeeded" || r.error?.kind === "stick_full") ? st.availableBytes : null,
        full_verified_at: st.fullVerifiedMs ? iso(st.fullVerifiedMs) : null,
        server_verify_failed: [...st.damagedPaths],
        password_reminder: this.faults.has("stick_password"),
        password_reminders: this.faults.has("stick_password") ? { [STICK_UUID]: "password_check_due" } : {},
        plugged_in: this.stickIn(),
        ...(this.flavor !== "today" ? {
          plugged_in_uuids: this.stickIn() ? this.faults.has("stick_multiple") ? [STICK_UUID, SECOND_STICK_UUID] : [STICK_UUID] : [],
          in_use: [...this.restores.values()].some((j) => j.stick && j.restore.state === "running") ? "restore" as const : running ? "backup" as const : null,
          sticks: Object.fromEntries((this.faults.has("stick_multiple") ? [STICK_UUID, SECOND_STICK_UUID] : [STICK_UUID]).map((uuid) => [uuid, {
            plugged_in: this.stickIn(),
            name: STICK_NAMES[uuid] ?? null,
            in_use: [...this.restores.values()].some((j) => j.stick && j.restore.stick_uuid === uuid && j.restore.state === "running") ? "restore" : running ? "backup" : null,
          }])),
        } : {}),
        total_bytes: 256e9,
        space_checked_at: last?.started_at ?? null,
      };
    }
    return out;
  }

  status(now = Date.now()): Status {
    // Like the daemon: the legacy fields describe the server, or the stick when it is the only target.
    const own = this.pbsTarget || !this.stick ? this.runs : this.stick.runs;
    const success = own.find((r) => r.state === "succeeded") ?? null;
    const last = own[0] ?? null;
    const overdueH = this.pbsTarget || !this.stick ? this.overdue() : this.stick.overdueHours;
    const health = this.healthOf(own, !!this.active, overdueH, now);
    const nextRun = this.nextRunOf(own, now);
    const targets = this.targets(now);
    const legacyDamaged = this.pbsTarget || !this.stick ? this.damagedPaths : this.stick.damagedPaths;
    return {
      ...(this.stick ? { targets } : {}),
      machine: { hostname: "desktop", backup_id: "desktop", os: "Linux Mint 22.2" },
      pbs: this.configured && this.pbsTarget
        ? {
            server: "pbs.example.com:8007",
            datastore: "backups",
            namespace: "desktops",
            reachable: this.reachable,
            checked_at: this.checkedMs ? iso(this.checkedMs) : null,
            wake_on_lan: this.wakeOn(),
            credentials_stored: this.faults.has("unknown_stored") ? null : this.credentialsStored,
          }
        : {
            server: "",
            datastore: "",
            namespace: null,
            reachable: null,
            checked_at: null,
            wake_on_lan: false,
            // No backup server: no token secret is stored (the daemon reports false).
            credentials_stored: this.faults.has("unknown_stored") ? null : false,
          },
      schedule: {
        enabled: this.configured && this.timerOn(),
        calendar: `every ${this.intervalHours} h (checked hourly)`,
        interval_hours: this.intervalHours,
        next_run: this.timerOn() ? nextRun : null,
      },
      current_run: this.strip(this.active?.run ?? null),
      last_run: this.strip(last),
      last_success: this.strip(success),
      health,
      overdue_after_hours: overdueH,
      encryption: {
        enabled: this.encryptionOn(),
        key_fingerprint: this.encryptionOn() && this.configured && this.keyCreated ? "<FINGERPRINT>" : null, // placeholder, see AGENTS.md
        key_exported: this.encryptionOn() && this.keyExported,
        key_present: this.faults.has("unknown_stored") ? null : this.configured && this.keyCreated,
      },
      ...(legacyDamaged.length ? { server_verify_failed: [...legacyDamaged] } : {}),
    };
  }

  private encryptionOn(): boolean {
    return !this.faults.has("no_encryption");
  }

  private timerOn(): boolean {
    return this.timerEnabled ?? !this.faults.has("schedule_off");
  }

  /** "Overdue after" as the configuration file has it. */
  private overdue(): number {
    return !this.overdueSet && this.faults.has("long_overdue") ? 744 : this.overdueHours;
  }

  /** Is the backup server switched off (what the daemon finds out by trying to reach it)? */
  private serverOff(): boolean {
    return this.reachable === false || this.faults.has("check_down");
  }

  private wakeConfigured(): boolean {
    return this.pbsTarget && !this.faults.has("no_wake");
  }

  private wakeOn(): boolean {
    return this.wakeConfigured() && this.wakeEnabled;
  }

  config(): ConfigView | null {
    if (!this.configured) return null;
    return {
      backup_id: "desktop",
      sources: [...ARCHIVES.map((a) => ({ archive: a.name, path: a.source })), ...this.extraSources],
      excludes: [...this.excludes],
      schedule: `every ${this.intervalHours} h`,
      interval_hours: this.intervalHours,
      encryption: this.encryptionOn(),
      overdue_after_hours: this.overdue(),
      stick_overdue_after_hours: this.stick ? this.stick.overdueHours : null,
      stick_data_uuids: this.stick ? (this.faults.has("stick_multiple") ? [STICK_UUID, SECOND_STICK_UUID] : [STICK_UUID]) : [],
      stick_names: this.stick ? STICK_NAMES : {},
      automatic_backups: this.timerOn(),
      wake_on_lan: this.wakeOn(),
      wake_on_lan_configured: this.wakeConfigured(),
      // Like `status::config_view`: the stick keys only with a stick, removing only with both targets.
      editable: SETTING_KEYS.filter((k) =>
        k === "wake_on_lan"
          ? this.wakeConfigured()
          : k === "stick_overdue_after_hours"
            ? !!this.stick
            : k === "remove_target"
              ? !!this.stick && this.pbsTarget
              : true,
      ),
      limits: this.limits(),
    };
  }

  /**
   * `limits` like the daemon: `max` is the cap for the rhythm in the file now (the larger of 168
   * and the rhythm plus 72 hours, and never below a larger value in the file, which can then only
   * be lowered); the page recomputes it from `base` and `per_interval_extra`.
   */
  private limits(): SettingsLimits {
    const file = this.overdue();
    const limits: SettingsLimits = {
      ...DEFAULT_LIMITS,
      overdue_after_hours: {
        ...DEFAULT_LIMITS.overdue_after_hours,
        base: OVERDUE_BASE_HOURS,
        per_interval_extra: OVERDUE_INTERVAL_EXTRA_HOURS,
        file_minimum: file,
      },
    };
    limits.overdue_after_hours.max = overdueCap(this.intervalHours, limits);
    limits.stick_overdue_after_hours = this.stick ? {
      ...limits.overdue_after_hours, min: 1, greater_than_interval: false,
      file_minimum: this.stick.overdueHours,
      max: Math.max(OVERDUE_BASE_HOURS, this.intervalHours + OVERDUE_INTERVAL_EXTRA_HOURS, this.stick.overdueHours),
    } : null;
    return limits;
  }

  /** `PATCH /config` like the daemon: validate everything, then apply all or nothing. */
  updateConfig(body: unknown): Reply {
    const err = (status: number, code: string, message: string): Reply => ({ status, body: { code, message } });
    if (!this.configured) return err(500, "internal", "cannot read configuration /etc/nestegg/config.toml: No such file or directory (os error 2)");
    const current = this.config()!;
    const sources = (current.sources ?? []).filter((x) => x.archive !== "inventory.pxar").map((x) => x.path);
    const problem = settingsProblem(
      body,
      {
        interval_hours: this.intervalHours,
        overdue_after_hours: this.overdue(),
        wake_on_lan_configured: this.wakeConfigured(),
        excludes: this.excludes,
        stick_overdue_after_hours: this.stick?.overdueHours ?? null,
        targets: { pbs: this.pbsTarget, stick: !!this.stick },
      },
      current.limits,
      (pattern) => malformedProblem(pattern) ?? wholeFolderProblem(pattern, sources),
    );
    if (problem) return err(400, "bad_request", problem);
    const change = body as SettingsChange;
    const timer = change.automatic_backups;
    if (timer !== undefined && timer !== null && timer !== this.timerOn() && this.faults.has("timer_fails")) {
      return err(
        500,
        "internal",
        `cannot switch automatic backups ${timer ? "on" : "off"}; nothing was changed: systemctl ${timer ? "enable" : "disable"} --now nestegg-backup.timer failed (exit status: 1): Failed to connect to bus: No such file or directory`,
      );
    }
    if (change.interval_hours != null) this.intervalHours = change.interval_hours;
    if (change.overdue_after_hours != null) {
      this.overdueHours = change.overdue_after_hours;
      this.overdueSet = true;
    }
    if (change.excludes != null) this.excludes = [...change.excludes];
    if (change.wake_on_lan != null && this.wakeConfigured()) this.wakeEnabled = change.wake_on_lan;
    if (timer != null) this.timerEnabled = timer;
    if (change.stick_overdue_after_hours != null && this.stick) this.stick.overdueHours = change.stick_overdue_after_hours;
    if (change.remove_target === "stick") this.stick = null;
    if (change.remove_target === "pbs") {
      // Like the daemon: [pbs] and [wake] go; the history of the server stays on disk.
      this.pbsTarget = false;
      this.reachable = null;
    }
    return { status: 200, body: this.config() };
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
    return { complete: true, machines, read_at: iso(Math.min(now, localAt(now, 0, 7, 20) < now ? localAt(now, 0, 7, 20) : now - 10 * 60_000)) };
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
    if (route === "GET /health") return { status: 200, body: { version: "0.1.0", api_version: today ? "1" : "2" } };
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
    if (route === "PATCH /config" && this.faults.has("config"))
      return err(500, "internal", "invalid configuration /etc/nestegg/config.toml; fix it in the terminal first: expected `=`");
    if (route === "GET /runs") {
      const limit = Math.min(500, Math.max(1, Number(query.get("limit") ?? 50) || 50));
      const target = this.pbsTarget ? "pbs" : "stick";
      const list = [...(this.active ? [this.active.run] : []), ...this.allRuns()]
        .filter((r) => (r.target ?? "pbs") === target).slice(0, limit).map((r) => this.strip(r));
      return { status: 200, body: { runs: list } };
    }
    if ((m = /^\/targets\/([^/]+)\/runs$/.exec(path)) && (method === "GET" || method === "POST")) {
      const target = decodeURIComponent(m[1]!);
      if (target !== "pbs" && target !== "stick" && target !== "all") return err(400, "invalid_target", "target must be pbs, stick or all");
      if (method === "POST") {
        const refused = this.startRun(target, now);
        return refused ?? { status: 202, body: this.strip(this.active!.run) };
      }
      const of = (r: Run) => target === "all" || (r.target ?? "pbs") === target;
      const list = [...(this.active ? [this.active.run] : []), ...this.allRuns()].filter(of).slice(0, 200).map((r) => this.strip(r));
      return { status: 200, body: { runs: list } };
    }
    if (route === "POST /runs") {
      const refused = this.startRun(this.pbsTarget ? "pbs" : "stick", now);
      return refused ?? { status: 202, body: this.strip(this.active!.run) };
    }
    if (method === "GET" && (m = /^\/runs\/([^/]+)$/.exec(path))) {
      const id = decodeURIComponent(m[1]!);
      const run = this.active?.run.id === id ? this.active.run : this.allRuns().find((r) => r.id === id);
      if (!run) return err(404, "not_found", "unknown run");
      return { status: 200, body: run satisfies RunDetail };
    }
    if (method === "POST" && (m = /^\/runs\/([^/]+)\/cancel$/.exec(path))) {
      const a = this.active;
      const id = decodeURIComponent(m[1]!);
      const known = a?.run.id === id || this.allRuns().some((r) => r.id === id);
      // Like the daemon: 404 for an id that does not exist, 409 `cannot_cancel` for a run that exists
      // but is over or has no process to signal yet. A daemon from before that (flavour `today`)
      // answered 404 "no running backup with this id" for all of these.
      const cannot = (message: string) =>
        today ? err(404, "not_found", "no running backup with this id") : err(409, "cannot_cancel", message);
      if (!known) return today ? err(404, "not_found", "no running backup with this id") : err(404, "not_found", "unknown run");
      if (!a || a.run.id !== id) return cannot("this backup has already finished and cannot be cancelled");
      const el = now - a.startedMs;
      if (this.faults.has("cancel_late")) return cannot("this backup has already finished and cannot be cancelled");
      if (el < 1_500) return cannot("the backup is still starting and cannot be cancelled yet; try again in a moment");
      // The daemon signals the run until it is gone, also while it finishes: it then ends as
      // cancelled, or as succeeded if the read-back check was already through.
      a.cancelAtMs ??= now;
      return { status: 202, body: { cancelling: true } };
    }
    if (route === "GET /pbs") {
      if (!this.configured || !this.pbsTarget) return notConfigured();
      if (today) return err(404, "not_found", "unknown endpoint");
      const info: PbsInfo = {
        server: "pbs.example.com:8007",
        datastore: "backups",
        namespace: "desktops",
        reachable: this.reachable === true,
        used_bytes: this.faults.has("no_usage") ? null : 1_240 * GB,
        total_bytes: this.faults.has("no_usage") ? null : 4_000 * GB,
        overview_configured: true,
      };
      return { status: 200, body: info };
    }
    if (route === "POST /pbs/check") {
      if (today) return err(404, "not_found", "unknown endpoint");
      // Like `check::run` of the daemon: it never fails. Problems are items with ok=false, also an
      // unreadable configuration or a server that does not answer (200, never 503). It does not
      // change what /status says about the server.
      const wakeParam = query.get("wake") === "true";
      const serverOff = this.serverOff();
      const delayMs = this.faults.has("check_slow") || wakeParam ? 6_000 : undefined;
      const checkedAt = iso(now + (delayMs ?? 0));
      if (!this.configured) {
        const none: PbsCheckResult = {
          checked_at: checkedAt,
          checks: [item("configuration", "failed", "cannot read /etc/nestegg/config.toml: No such file or directory (os error 2)")],
        };
        return { status: 200, body: none, delayMs };
      }
      const checks: PbsCheckResult["checks"] = [
        item("configuration", "passed", "/etc/nestegg/config.toml"),
        item("proxmox-backup-client", "passed", "proxmox-backup-client 3.4.1"),
        this.credentialsStored
          ? item("password / token secret", "passed", "")
          : item("password / token secret", "failed", "cannot read /etc/nestegg/credentials/password.cred: No such file or directory (os error 2)"),
      ];
      if (this.encryptionOn()) {
        checks.push(
          this.keyCreated
            ? item("encryption key", "passed", "")
            : item("encryption key", "failed", "cannot read /etc/nestegg/credentials/encryption-key.cred: No such file or directory (os error 2)"),
          item(
            "encryption key exported",
            this.keyExported ? "passed" : "failed",
            this.keyExported ? "" : "export the key, then run `nestegg key confirm-exported --file <copy>` (or `--paper-code <code>`)",
          ),
        );
      }
      const sources = [
        ...ARCHIVES.map((a) => ({ archive: a.name.replace(/\.pxar$/, ""), path: a.source })),
        ...this.extraSources.map((e) => ({ archive: e.archive.replace(/\.pxar$/, ""), path: e.path })),
      ];
      for (const src of sources) {
        const missing = src.archive === "fotos" && this.scenario === "failing";
        checks.push(item(`source ${src.archive}`, missing ? "failed" : "passed", `${src.path}${missing ? " (missing)" : ""}`));
      }
      // Like `check::run`: a server that does not answer is a failed item without Wake-on-LAN. With it,
      // and without a wake request, nothing is known ("not_checked"); after a wake attempt (wake=true)
      // that finds nobody, it is a failed item. What needs the server is "not_checked" then.
      const wakeOn = this.wakeOn();
      if (!serverOff) {
        checks.push(item("PBS reachable", "passed", "pbs.example.com:8007"));
      } else if (wakeOn && wakeParam) {
        checks.push(item("PBS reachable", "failed", "no answer, not even after the Wake-on-LAN attempt"));
      } else if (wakeOn) {
        checks.push(
          item(
            "PBS reachable",
            "not_checked",
            "no answer; the server may be asleep (Wake-on-LAN is configured, a backup run wakes it by itself)",
          ),
        );
      } else {
        checks.push(item("PBS reachable", "failed", "no answer"));
      }
      if (serverOff) {
        const why = wakeOn && !wakeParam ? "not checked: the server did not answer" : "not checked: the server is not reachable";
        checks.push(item("PBS login and datastore access", "not_checked", why), item("overview access", "not_checked", why));
      } else {
        checks.push(
          this.faults.has("check_bad")
            ? item("PBS login and datastore access", "failed", "authentication failed: 401 invalid credentials for backup@pbs!desktop")
            : item("PBS login and datastore access", "passed", `${this.snapshots.length} snapshots`),
          item("overview access", "passed", "4 machines"),
        );
      }
      checks.push(item("backup timer enabled", this.timerOn() ? "passed" : "failed", "nestegg-backup.timer"));
      const body: PbsCheckResult = { checked_at: checkedAt, checks };
      return { status: 200, body, delayMs };
    }
    // Restoring from the stick (`target=stick`). A daemon from before that (`today`) ignores the parameter.
    const fromStick = query.get("target") === "stick" || (body as Partial<RestoreRequest> | null)?.target === "stick";
    if (fromStick && !today && (route === "GET /snapshots" || route === "POST /restores" || /^GET \/snapshots\/[^/]+\/browse$/.test(route))) {
      const slow = (r: Reply, ms: number): Reply => ({ ...r, delayMs: ms });
      if (!this.configured) return notConfigured();
      const unavailable = (reason: string, message: string): Reply => ({ status: 503, body: { code: "stick_unavailable", reason, message } });
      const uuid = query.get("stick_uuid") ?? (body as Partial<RestoreRequest> | null)?.stick_uuid;
      const selected = uuid ?? STICK_UUID;
      if (uuid && ![STICK_UUID, ...(this.faults.has("stick_multiple") ? [SECOND_STICK_UUID] : [])].includes(uuid)) return err(400, "bad_request", "stick UUID is not registered");
      if (!uuid && this.faults.has("stick_multiple")) return unavailable("ambiguous", "choose a stick to restore from");
      const echo = { target: "stick" as const, stick_uuid: selected };
      if (!this.stick) return unavailable("unsupported", "no stick target is configured");
      if (!this.stickIn()) return slow(unavailable("not_plugged_in", "registered stick not plugged in"), 600);
      if (this.faults.has("stick_busy")) return slow(unavailable("unreadable", "the stick filesystem could not be read"), 1_500);
      if (this.active || [...this.restores.values()].some((j) => j.restore.state === "running"))
        return unavailable("busy", "a backup or stick restore is running");
      const list = this.stickSnapshots();
      if (route === "GET /snapshots") return slow({ status: 200, body: { ...echo, snapshots: list } }, 1_800);
      if (method === "GET") {
        const time = decodeURIComponent(/^\/snapshots\/([^/]+)\/browse$/.exec(path)![1]!);
        if (!list.some((s) => s.time === time)) return unavailable("snapshot_not_on_stick", `snapshot ${time} not found on this stick`);
        const entries = browseTree(query.get("archive") ?? "", query.get("path") ?? "/");
        if (!entries) return err(404, "not_found", "no such directory");
        return slow({ status: 200, body: { ...echo, entries } }, 700);
      }
      const req = body as Partial<RestoreRequest>;
      const many = Array.isArray(req.archives) && req.archives.length > 0;
      if (!req.snapshot_time || !req.target_dir || (!many && (!req.archive || !Array.isArray(req.paths))))
        return err(400, "bad_request", "snapshot_time, archive (or archives), paths and target_dir are required");
      if (!list.some((s) => s.time === req.snapshot_time)) return unavailable("snapshot_not_on_stick", `snapshot ${req.snapshot_time} not found on this stick`);
      if (EXISTING_TARGETS.includes(req.target_dir.replace(/\/$/, "")))
        return err(409, "target_exists", `target directory ${req.target_dir} already exists`);
      const id = `restore-${this.restores.size + 1}`;
      const restore: Restore = {
        ...echo,
        id,
        state: "running",
        target_dir: req.target_dir,
        started_at: iso(now),
        finished_at: null,
        error: null,
        restored_bytes: 0,
        total_bytes: null,
        warnings: [],
      };
      const whole = many ? req.archives!.every((a) => !a.paths?.length) : req.paths!.length === 0;
      this.restores.set(id, { restore, startedMs: now, totalBytes: (many ? req.archives!.length : 1) * 1.7 * GB, stick: true, wholeArchives: whole });
      return slow({ status: 202, body: restore }, 1_500);
    }
    if (route === "GET /snapshots") {
      if (!this.configured) return notConfigured();
      // Like the daemon: without [pbs] the reachability probe fails.
      if (this.reachable === false || !this.pbsTarget) return err(503, "pbs_unreachable", "PBS is not reachable");
      return { status: 200, body: { ...(today ? {} : { target: "pbs", stick_uuid: null }), snapshots: this.snapshots } };
    }
    if (method === "GET" && (m = /^\/snapshots\/([^/]+)\/browse$/.exec(path))) {
      if (today) return err(501, "not_implemented", "planned for v0.2");
      if (this.reachable === false) return err(503, "pbs_unreachable", "PBS is not reachable");
      const time = decodeURIComponent(m[1]!);
      if (!this.snapshots.some((s) => s.time === time)) return err(404, "not_found", "unknown snapshot");
      const entries = browseTree(query.get("archive") ?? "", query.get("path") ?? "/");
      if (!entries) return err(404, "not_found", "no such directory");
      return { status: 200, body: { target: "pbs", stick_uuid: null, entries } };
    }
    if (route === "POST /restores") {
      if (today) return err(501, "not_implemented", "planned for v0.2");
      const req = body as Partial<RestoreRequest> | null;
      const many = Array.isArray(req?.archives) && req.archives.length > 0;
      if (!req || !req.snapshot_time || !req.target_dir || (!many && (!req.archive || !Array.isArray(req.paths))))
        return err(400, "bad_request", "snapshot_time, archive (or archives), paths and target_dir are required");
      if (many && new Set(req.archives!.map((a) => a.archive)).size !== req.archives!.length)
        return err(400, "bad_request", "an archive may be listed only once");
      if (!req.target_dir.startsWith("/")) return err(400, "bad_request", "target_dir must be an absolute path");
      if (EXISTING_TARGETS.includes(req.target_dir.replace(/\/$/, "")))
        return err(409, "target_exists", `target directory ${req.target_dir} already exists`);
      const id = `restore-${this.restores.size + 1}`;
      const totalBytes = (many ? req.archives!.length : 1) * 3.2 * GB;
      const restore: Restore = {
        target: "pbs",
        stick_uuid: null,
        id,
        state: "running",
        target_dir: req.target_dir,
        started_at: iso(now),
        finished_at: null,
        error: null,
        restored_bytes: 0,
        total_bytes: (req.paths?.length ?? 1) === 0 || many ? Math.round(totalBytes) : null,
        warnings: [],
      };
      this.restores.set(id, { restore, startedMs: now, totalBytes });
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
    if (route === "PATCH /config") return this.updateConfig(body);
    // Proposed endpoints (API-REQUESTS.md).
    // Like the daemon: it reads the server first, which needs the configuration (500 without it).
    if (route === "GET /machines" && !today) {
      if (!this.configured) return notConfigured();
      // The daemon probes the server first (3 s) and answers 503 when nobody answers.
      if (this.serverOff() || !this.pbsTarget) return err(503, "pbs_unreachable", "PBS is not reachable");
      return { status: 200, body: this.machines(now) };
    }
    if (route === "GET /session" && !today) return { status: 200, body: { user: "alice", home: "/home/alice" } };
    return err(404, "not_found", "unknown endpoint");
  }
}
