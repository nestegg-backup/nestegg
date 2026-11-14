// Types of the hub API, see ../../../api/hub-openapi.yaml (single source of truth).
// Unknown fields are ignored; fields the contract allows to be null are nullable here.

import type { MachineReport } from "../lib/types";

/** `GET /hub`: state of the hub. */
export interface HubInfo {
  /** Name of the household. */
  name: string;
  version: string;
  /** Address computers use to reach the hub. */
  url: string;
  pin: string;
  pbs: {
    server: string;
    port: number;
    datastore: string;
    namespace: string | null;
    user: string;
    wake_on_lan: boolean;
  };
  master_key: {
    present: boolean;
    /** The copy outside the hub was proven (decision 0018). */
    proven: boolean;
    proven_at?: string | null;
    fingerprint?: string | null;
  };
  package: {
    available: boolean;
    version?: string | null;
    architecture?: string | null;
  };
}

/**
 * The status report a computer stored in the notes of its newest snapshot (`StatusReport` of
 * `api/openapi.yaml`). Written by that computer, possibly a newer version: display only.
 */
export interface HubReport extends MachineReport {
  incomplete?: string[];
  verified?: boolean;
}

/** One joined computer with what the PBS knows about its backups. */
export interface HubMachine {
  machine_id: string;
  /** Name of its backups on the PBS. */
  backup_id: string;
  hostname: string;
  os?: string | null;
  nestegg_version?: string | null;
  joined_at: string;
  snapshot_count: number;
  last_snapshot?: string | null;
  /** The PBS's own verification of the newest snapshot; null: not verified yet. */
  last_snapshot_verified?: boolean | null;
  report?: HubReport | null;
}

/** A backup group in the household namespace that belongs to no joined computer. */
export interface UnmanagedGroup {
  backup_id: string;
  last_snapshot?: string | null;
}

/** `GET /machines`. */
export interface HubMachines {
  /** false: the PBS did not answer now (asleep); the data below is what the hub read last. */
  pbs_reachable: boolean;
  /** When the PBS data was read; null: never. */
  pbs_read_at: string | null;
  machines: HubMachine[];
  unmanaged: UnmanagedGroup[];
}

/** A one-time code that is still valid (the code itself is never listed). */
export interface JoinCodeInfo {
  code_id: string;
  note?: string | null;
  created_at: string;
  expires_at: string;
}

/** `POST /join-codes`: the only answer that contains the code and the command line. */
export interface JoinCode extends JoinCodeInfo {
  code: string;
  command: string;
}
