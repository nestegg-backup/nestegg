// Application state and polling.

import { api, ApiFailure, optional } from "./api";
import { initBrowserSession } from "./session";
import { isActive } from "./derive";
import type { ConfigView, HealthInfo, MachinesResponse, Run, SessionInfo, Snapshot, Status } from "./types";

const POLL_RUNNING_MS = 2_000;
const POLL_IDLE_MS = 30_000;
/** While the daemon does not answer, try again more often so the page recovers quickly. */
const POLL_OFFLINE_MS = 5_000;
const MACHINES_MS = 10 * 60_000;

function asFailure(e: unknown): ApiFailure {
  return e instanceof ApiFailure ? e : new ApiFailure("other", 0, null, e instanceof Error ? e.message : String(e));
}

/** Data that is loaded separately from /status. Failures are kept, never shown as empty data. */
export interface Loadable<T> {
  state: "loading" | "ok" | "error";
  value: T;
  error: ApiFailure | null;
}

class AppState {
  status = $state<Status | null>(null);
  /** Error of the last status request. With a cached `status` this means "connection lost". */
  fatal = $state<ApiFailure | null>(null);
  /** Time of the last successful status request. */
  lastContact = $state<Date | null>(null);
  runs = $state<Loadable<Run[]>>({ state: "loading", value: [], error: null });
  config = $state<Loadable<ConfigView | null>>({ state: "loading", value: null, error: null });
  /** undefined: not offered by this daemon (404/501); null: not loaded yet. */
  machines = $state<MachinesResponse | null | undefined>(null);
  machinesError = $state<ApiFailure | null>(null);
  healthInfo = $state<HealthInfo | null>(null);
  session = $state<SessionInfo | null | undefined>(null);
  snapshots = $state<{ state: "idle" | "loading" | "ok" | "error"; list: Snapshot[]; error: ApiFailure | null }>({
    state: "idle",
    list: [],
    error: null,
  });
  now = $state(new Date());
  starting = $state(false);
  startError = $state<{ kind: "conflict" | "other"; message: string } | null>(null);

  #timer: ReturnType<typeof setTimeout> | null = null;
  #started = false;

  get running(): boolean {
    return isActive(this.status?.current_run);
  }

  /** Cached data is shown, but the daemon does not answer right now. */
  get offline(): boolean {
    return !!this.status && !!this.fatal;
  }

  start() {
    if (this.#started) return;
    this.#started = true;
    void this.poll();
    void this.loadRuns();
    void this.loadConfig();
    void this.loadMachines();
    api.health().then((h) => (this.healthInfo = h), () => {});
    optional(api.session()).then((s) => (this.session = s), () => (this.session = undefined));
    setInterval(() => (this.now = new Date()), 15_000);
    setInterval(() => void this.loadMachines(), MACHINES_MS);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") void this.poll();
    });
    // The address printed by `nestegg ui` opened again in this tab: new session.
    window.addEventListener("hashchange", () => {
      if (initBrowserSession()) {
        this.retry();
        void this.loadMachines();
      }
    });
  }

  async poll() {
    if (this.#timer) clearTimeout(this.#timer);
    this.#timer = null;
    const before = this.status;
    const wasRunning = this.running;
    const wasOffline = !!this.fatal;
    try {
      this.status = await api.status();
      this.fatal = null;
      this.lastContact = new Date();
    } catch (e) {
      this.fatal = asFailure(e);
    }
    this.now = new Date();
    const after = this.status;
    if (!this.fatal && after) {
      // A run may have started or finished between two polls (timer, CLI, another browser tab):
      // refresh the history and the snapshot list whenever the run ids change.
      const changed =
        before?.last_run?.id !== after.last_run?.id ||
        before?.last_success?.id !== after.last_success?.id ||
        before?.current_run?.id !== after.current_run?.id;
      if (changed || this.running || wasRunning || wasOffline) void this.loadRuns();
      if (wasOffline && this.config.state === "error") void this.loadConfig();
      if (before && before.last_success?.id !== after.last_success?.id && this.snapshots.state !== "idle") {
        void this.loadSnapshots(true);
      }
    }
    if (typeof document !== "undefined" && document.visibilityState === "hidden" && !this.running) return;
    // A 401 does not heal by waiting: the session ended. Polling resumes when a new
    // token arrives (hashchange) or the user retries.
    if (this.fatal?.kind === "unauthorized") return;
    const delay = this.fatal ? POLL_OFFLINE_MS : this.running ? POLL_RUNNING_MS : POLL_IDLE_MS;
    this.#timer = setTimeout(() => void this.poll(), delay);
  }

  retry() {
    void this.poll();
    void this.loadRuns();
    void this.loadConfig();
  }

  async loadRuns() {
    if (this.runs.state !== "ok") this.runs = { ...this.runs, state: "loading", error: null };
    try {
      this.runs = { state: "ok", value: await api.runs(), error: null };
    } catch (e) {
      // Keep a list loaded earlier (it stays correct up to its age); only a
      // never-loaded list becomes an error state.
      if (this.runs.state !== "ok") this.runs = { state: "error", value: [], error: asFailure(e) };
    }
  }

  async loadConfig() {
    if (this.config.state !== "ok") this.config = { ...this.config, state: "loading", error: null };
    try {
      this.config = { state: "ok", value: await api.config(), error: null };
    } catch (e) {
      if (this.config.state !== "ok") this.config = { state: "error", value: null, error: asFailure(e) };
    }
  }

  async loadMachines() {
    try {
      this.machines = await optional(api.machines());
      this.machinesError = null;
    } catch (e) {
      // Shown as a note in the list; never breaks the page of this PC.
      this.machinesError = asFailure(e);
    }
  }

  async loadSnapshots(force = false) {
    // Only automatically once; after an error the user retries explicitly.
    if (!force && this.snapshots.state !== "idle") return;
    this.snapshots = { state: "loading", list: this.snapshots.list, error: null };
    try {
      const list = await api.snapshots();
      list.sort((a, b) => b.time.localeCompare(a.time));
      this.snapshots = { state: "ok", list, error: null };
    } catch (e) {
      this.snapshots = { state: "error", list: [], error: asFailure(e) };
    }
  }

  async startRun() {
    this.starting = true;
    this.startError = null;
    try {
      await api.startRun();
    } catch (e) {
      const f = asFailure(e);
      this.startError = { kind: f.kind === "conflict" ? "conflict" : "other", message: f.message };
    }
    await this.poll();
    this.starting = false;
  }
}

export const app = new AppState();
