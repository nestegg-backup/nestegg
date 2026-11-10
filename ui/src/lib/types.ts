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
  | "cancelled"
  /** Written by a newer version of nestegg that this one does not understand (forward compatibility). */
  | "unknown";

/** `timer`, `manual`, `cli`; `unknown` is a value written by a newer version. */
export type RunTrigger = "timer" | "manual" | "cli" | "unknown";

export type RunErrorKind =
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

export interface Status {
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
export type SettingKey = "interval_hours" | "overdue_after_hours" | "excludes" | "wake_on_lan" | "automatic_backups";

/** Body of `PATCH /api/v1/config`: absent fields stay unchanged. */
export interface SettingsChange {
  interval_hours?: number;
  /** Must be greater than `interval_hours`. */
  overdue_after_hours?: number;
  /** The complete new list (replaces the old one). */
  excludes?: string[];
  wake_on_lan?: boolean;
  automatic_backups?: boolean;
}

export interface SettingsLimits {
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
  result: "succeeded" | "failed" | "pbs_unreachable";
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
