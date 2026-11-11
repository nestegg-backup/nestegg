// Types of the local API, see ../../api/openapi.yaml (single source of truth).
// Fields the contract marks as optional are optional here; unknown fields are ignored.

export type Health = "ok" | "running" | "overdue" | "failing" | "incomplete" | "never" | "unconfigured";

export type RunState =
  | "starting"
  | "waking_pbs"
  | "inventory"
  | "uploading"
  | "finishing"
  | "succeeded"
  | "failed"
  | "pbs_unreachable"
  /** A stick run found no registered stick. Not a failure: it neither counts nor resets the failure counter. */
  | "stick_not_plugged_in"
  | "cancelled"
  /** Written by a newer version of nestegg that this one does not understand (forward compatibility). */
  | "unknown";

/** `timer`, `manual`, `cli`, `plug` (a registered stick was plugged in); `unknown` is a value written by a newer version. */
export type RunTrigger = "timer" | "manual" | "cli" | "plug" | "unknown";

/** Where a backup goes: the backup server (PBS) or a registered stick. */
export type Target = "pbs" | "stick";

export type RunErrorKind =
  | "stick_not_plugged_in"
  /** Too little space left on the stick for the PBS backup user; nothing beyond the retention rules was deleted. */
  | "stick_full"
  | "stick_already_mounted"
  | "stick_ambiguous"
  | "stick_busy"
  /** The offline check of the stick's ext4 file system found damage; it needs an offline repair. */
  | "filesystem_damaged"
  /** This computer cannot start the stick's local backup server (kernel IPv6 support missing, no root helper). */
  | "stick_unsupported"
  /** The newest backup of this computer's group on the target was made with another key. */
  | "key_mismatch"
  | "pbs_unreachable"
  | "authentication"
  | "source_missing"
  | "client_missing"
  | "client_failed"
  | "already_running"
  | "config"
  | "credentials_unreadable"
  | "verification_failed"
  | "cancelled"
  | "internal"
  /** Written by a newer version of nestegg that this one does not understand (forward compatibility). */
  | "unknown";

export interface RunError {
  kind: RunErrorKind;
  message: string;
}

export interface Archive {
  name: string;
  source: string;
  size_bytes?: number | null;
}

export interface Run {
  /** Absent in records of older versions: those belong to the backup server. */
  target?: Target;
  /** The physical stick a stick run used. */
  stick_uuid?: string | null;
  id: string;
  trigger: RunTrigger;
  state: RunState;
  started_at: string;
  finished_at?: string | null;
  duration_seconds?: number | null;
  snapshot?: string | null;
  archives?: Archive[];
  uploaded_bytes?: number | null;
  /** Total archive size of the last successful run (progress estimate). */
  expected_bytes?: number | null;
  /** Total size of the archives finished so far in this run. */
  processed_bytes?: number | null;
  warnings?: string[];
  /**
   * Why this backup does not contain everything the configuration asks for
   * (English text; every entry is also in `warnings`). Empty or absent for a complete backup.
   */
  incomplete?: string[];
  /** The snapshot was read back from the server after the upload. Absent in records of older versions. */
  verified?: boolean;
  error?: RunError | null;
}

export interface RunDetail extends Run {
  log_tail?: string[];
}

/** One configured backup target in `/status` (`targets`). */
export interface TargetStatus {
  health: Health;
  /** Like `health`, or a target-specific state: `stick_full`, `stick_not_plugged_in`. Other words may follow. */
  state: Health | "stick_full" | "stick_not_plugged_in" | (string & {});
  last_run?: Run | null;
  last_success?: Run | null;
  current_run?: Run | null;
  consecutive_failures: number;
  /** Stick default: 168 hours (7 days). */
  overdue_after_hours: number;
  next_run?: string | null;
  /** Free space for the PBS backup user, as found at the last run of this target. */
  available_bytes?: number | null;
  total_bytes?: number | null;
  space_checked_at?: string | null;
  last_checked_at?: string | null;
  /** Live presence, independent of a previous run's outcome; null means unknown. */
  plugged_in?: boolean | null;
  /** Last complete verification of the whole stick. */
  full_verified_at?: string | null;
  server_verify_failed?: string[];
  /** A registered stick has no password proof yet, or its 180-day password re-check is due (root-only CLI). */
  password_reminder: boolean;
  /** Only media needing attention; absence means the proof is current. */
  password_reminders?: Record<string, "key_copy_missing" | "password_check_due">;
}

export interface Status {
  /** Only the configured targets; older daemons leave it out (then there is only the backup server). */
  targets?: Partial<Record<Target, TargetStatus>>;
  /** Legacy fields below describe the backup server when one is configured, otherwise the stick. */
  machine: { hostname: string; backup_id: string; os?: string | null };
  pbs: {
    server: string;
    datastore: string;
    namespace?: string | null;
    reachable?: boolean | null;
    checked_at?: string | null;
    /** Wake-on-LAN is configured and switched on. */
    wake_on_lan?: boolean;
    /** The token secret (or password) is stored on this computer. null or absent: unknown. */
    credentials_stored?: boolean | null;
  };
  schedule: { enabled: boolean; calendar?: string; interval_hours?: number; next_run?: string | null };
  current_run?: Run | null;
  last_run?: Run | null;
  last_success?: Run | null;
  health: Health;
  overdue_after_hours?: number;
  encryption: {
    enabled: boolean;
    key_fingerprint?: string | null;
    key_exported?: boolean;
    /** An encryption key is stored on this computer (it may still be unconfirmed). null or absent: unknown. */
    key_present?: boolean | null;
  };
  /** Snapshots of this machine (`host/desktop/2026-11-08T21:30:12Z`) that the server's own check marked as failed. */
  server_verify_failed?: string[];
}

export interface Snapshot {
  time: string;
  snapshot: string;
  size_bytes?: number | null;
  archives?: string[];
  encrypted?: boolean | null;
  verified?: "ok" | "failed" | null;
  protected?: boolean;
  comment?: string | null;
}

export interface DirEntry {
  name: string;
  type: "file" | "directory" | "symlink" | "other";
  size_bytes?: number | null;
  modified?: string | null;
}

/** Either `archive` (+ `paths`) or `archives` (takes precedence; each goes into its own subfolder). */
export interface RestoreRequest {
  snapshot_time: string;
  archive?: string;
  paths?: string[];
  archives?: { archive: string; paths: string[] }[];
  target_dir: string;
}

export interface Restore {
  id: string;
  state: "running" | "succeeded" | "failed";
  target_dir?: string;
  started_at?: string;
  finished_at?: string | null;
  error?: string | null;
  /** Bytes restored so far (staging copy). */
  restored_bytes?: number;
  /** Expected total when whole archives are restored. */
  total_bytes?: number | null;
  warnings?: string[];
}

export interface ConfigView {
  backup_id?: string;
  sources?: { archive: string; path: string }[];
  excludes?: string[];
  schedule?: string;
  interval_hours?: number;
  encryption?: boolean;
  overdue_after_hours?: number;
  /** "Overdue after" of the stick target; null or absent without a stick. */
  stick_overdue_after_hours?: number | null;
  stick_data_uuids?: string[];
  /** `nestegg-backup.timer` is enabled. */
  automatic_backups?: boolean;
  /** Wake-on-LAN is configured and switched on. */
  wake_on_lan?: boolean;
  /** A `[wake]` section with a MAC address exists; only then can `wake_on_lan` be switched on. */
  wake_on_lan_configured?: boolean;
  /** Fields `PATCH /config` accepts (older daemons: absent, nothing is editable). */
  editable?: SettingKey[];
  limits?: SettingsLimits;
}

/** Settings the web UI may change (`PATCH /api/v1/config`, decision 0013). */
export type SettingKey =
  | "interval_hours"
  | "overdue_after_hours"
  | "stick_overdue_after_hours"
  | "remove_target"
  | "excludes"
  | "wake_on_lan"
  | "automatic_backups";

/** Body of `PATCH /api/v1/config`: absent fields stay unchanged. */
export interface SettingsChange {
  /** "Overdue after" of the stick (1 to the same cap as the server's). Raising it is reported (decision 0022). */
  stick_overdue_after_hours?: number;
  /** Remove one of two configured targets; the last one cannot be removed. Reported (decision 0022). */
  remove_target?: Target;
  interval_hours?: number;
  /** Must be greater than `interval_hours`. */
  overdue_after_hours?: number;
  /** The complete new list (replaces the old one). */
  excludes?: string[];
  wake_on_lan?: boolean;
  automatic_backups?: boolean;
}

export interface SettingsLimits {
  stick_overdue_after_hours?: SettingsLimits["overdue_after_hours"] | null;
  interval_hours: { min: number; max: number };
  overdue_after_hours: {
    min: number;
    /** The cap for the rhythm that is in the file now (or the larger value the file already has). */
    max: number;
    greater_than_interval?: boolean;
    /** The cap is the larger of `base` and the rhythm plus `per_interval_extra` (decision 0022). */
    base?: number;
    per_interval_extra?: number;
    /** The value in the file now: a value above the cap can only be lowered. */
    file_minimum?: number;
  };
  excludes: { max_items: number; max_length: number };
}

/** `GET /api/v1/pbs`: facts about the backup server. */
export interface PbsInfo {
  server: string;
  datastore: string;
  namespace?: string | null;
  reachable: boolean;
  /** Needs read access to the datastore status; null otherwise. */
  used_bytes?: number | null;
  total_bytes?: number | null;
  overview_configured: boolean;
}

/** Result of `POST /api/v1/pbs/check`. */
export type CheckStatus = "passed" | "failed" | "not_checked";

export interface PbsCheckItem {
  name: string;
  /** `status == passed`; a check that was not made has `ok: false`. */
  ok: boolean;
  /** Absent in answers of older daemons: `ok` then means passed or failed. */
  status?: CheckStatus;
  detail: string;
}

export interface PbsCheckResult {
  checked_at: string;
  checks: PbsCheckItem[];
}

export interface HealthInfo {
  version: string;
  api_version: string;
}

/** `GET /api/v1/machines`: other machines of the household, read from the PBS. */
export interface Machine {
  backup_id: string;
  hostname?: string | null;
  os?: string | null;
  last_snapshot?: string | null;
  /** Snapshot times of the last 14 days, newest first. */
  recent_snapshots?: string[];
  size_bytes?: number | null;
  report?: MachineReport | null;
}

export interface MachineReport {
  reported_at: string;
  /** Written by the other PC's nestegg, possibly a newer version: any other word counts as "unknown". */
  result: "succeeded" | "failed" | "pbs_unreachable" | "unknown";
  run_started_at?: string | null;
  error?: RunError | null;
  key_exported?: boolean | null;
  sources?: string[];
  overdue_after_hours?: number | null;
  nestegg_version?: string | null;
}

export interface MachinesResponse {
  machines: Machine[];
  read_at?: string | null;
  /** false if only this machine's own backups are visible (no overview credentials). */
  complete?: boolean;
}

/** `GET /api/v1/session`: the user that runs `nestegg ui`. */
export interface SessionInfo {
  user: string;
  home: string;
  uid?: number;
  root?: boolean;
}
