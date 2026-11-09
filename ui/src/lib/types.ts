// Types of the local API, see ../../api/openapi.yaml (single source of truth).
// Fields the contract marks as optional are optional here; unknown fields are ignored.

export type Health = "ok" | "running" | "overdue" | "failing" | "never" | "unconfigured";

export type RunState =
  | "starting"
  | "waking_pbs"
  | "inventory"
  | "uploading"
  | "finishing"
  | "succeeded"
  | "failed"
  | "pbs_unreachable"
  | "cancelled";

export type RunErrorKind =
  | "pbs_unreachable"
  | "authentication"
  | "source_missing"
  | "client_missing"
  | "client_failed"
  | "already_running"
  | "config"
  | "credentials_unreadable"
  | "internal";

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
  trigger: "timer" | "manual" | "cli";
  state: RunState;
  started_at: string;
  finished_at?: string | null;
  duration_seconds?: number | null;
  snapshot?: string | null;
  archives?: Archive[];
  uploaded_bytes?: number | null;
  warnings?: string[];
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
  };
  schedule: { enabled: boolean; calendar?: string; interval_hours?: number; next_run?: string | null };
  current_run?: Run | null;
  last_run?: Run | null;
  last_success?: Run | null;
  health: Health;
  overdue_after_hours?: number;
  encryption: { enabled: boolean; key_fingerprint?: string | null; key_exported?: boolean };
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

export interface RestoreRequest {
  snapshot_time: string;
  archive: string;
  paths: string[];
  target_dir: string;
}

export interface Restore {
  id: string;
  state: "running" | "succeeded" | "failed";
  target_dir?: string;
  started_at?: string;
  finished_at?: string | null;
  error?: string | null;
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
  overdue_after_hours: { min: number; max: number; greater_than_interval?: boolean };
  excludes: { max_items: number; max_length: number };
}

export interface HealthInfo {
  version: string;
  api_version: string;
}

// ---- Proposed endpoints (see ../API-REQUESTS.md); not in the contract yet. ----

/** Proposed `GET /api/v1/machines`: other machines of the household, read from the PBS. */
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
}

/** Proposed `GET /api/v1/session`: the user that runs `nestegg ui`. */
export interface SessionInfo {
  user: string;
  home: string;
}
