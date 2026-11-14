// State of the hub page: the session, the data and the polling.
//
// A 401 on any request (except the login itself) means the session has ended:
// the data is dropped and the login is shown again (`api.ts` calls `ended`).

import { asHubFailure, hubApi, setUnauthorizedHandler, type HubFailure } from "./api";
import type { HubInfo, HubMachines, JoinCodeInfo } from "./types";

/** The list while nothing is open. */
const POLL_IDLE_MS = 60_000;
/** While "Add computer" is open: the new computer appears by itself. */
const POLL_FAST_MS = 5_000;
const CLOCK_MS = 5_000;

export type Phase = "checking" | "login" | "in";

class HubState {
  phase = $state<Phase>("checking");
  /** The login is shown because a session ended (not on the first visit). */
  ended = $state(false);
  /** The first `GET /session` got no answer: the hub itself is not reachable. */
  startError = $state<HubFailure | null>(null);
  hub = $state<HubInfo | null>(null);
  hubError = $state<HubFailure | null>(null);
  machines = $state<HubMachines | null>(null);
  machinesError = $state<HubFailure | null>(null);
  codes = $state<JoinCodeInfo[] | null>(null);
  codesError = $state<HubFailure | null>(null);
  now = $state(new Date());

  #fast = 0;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #started = false;

  /** Cached data is shown, but the hub did not answer the last request. */
  get offline(): boolean {
    return !!this.machines && this.machinesError?.status === 0;
  }

  start() {
    if (this.#started) return;
    this.#started = true;
    setUnauthorizedHandler(() => this.endSession());
    setInterval(() => (this.now = new Date()), CLOCK_MS);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible" && this.phase === "in") void this.refresh();
    });
    void this.checkSession();
  }

  async checkSession() {
    this.startError = null;
    try {
      const s = await hubApi.session();
      if (s.logged_in) this.enter();
      else this.phase = "login";
    } catch (e) {
      this.startError = asHubFailure(e);
      // The hub answered, but not with a session: the login decides.
      if (this.startError.status !== 0) this.phase = "login";
    }
  }

  /** Rejects with the hub's failure (401 wrong password, 429 `rate_limited`, 0 no answer). */
  async login(password: string) {
    await hubApi.login(password);
    this.ended = false;
    this.enter();
  }

  async logout() {
    try {
      await hubApi.logout();
    } finally {
      this.clear();
      this.ended = false;
      this.phase = "login";
    }
  }

  endSession() {
    if (this.phase !== "in") return;
    this.clear();
    this.ended = true;
    this.phase = "login";
  }

  private clear() {
    if (this.#timer) clearTimeout(this.#timer);
    this.#timer = null;
    // An open dialog goes away with the page; it cannot close its fast polling any more.
    this.#fast = 0;
    this.hub = null;
    this.hubError = null;
    this.machines = null;
    this.machinesError = null;
    this.codes = null;
    this.codesError = null;
  }

  private enter() {
    this.phase = "in";
    this.startError = null;
    void this.loadHub();
    void this.refresh();
  }

  /** Ask more often while the "Add computer" dialog is open. Calls nest (open/close pairs). */
  setFast(on: boolean) {
    this.#fast = Math.max(0, this.#fast + (on ? 1 : -1));
    if (on && this.phase === "in") void this.refresh();
  }

  private schedule() {
    if (this.#timer) clearTimeout(this.#timer);
    if (this.phase !== "in") return;
    this.#timer = setTimeout(() => void this.refresh(), this.#fast > 0 ? POLL_FAST_MS : POLL_IDLE_MS);
  }

  async refresh() {
    await Promise.all([this.loadMachines(), this.loadCodes()]);
    this.schedule();
  }

  async loadHub() {
    try {
      this.hub = await hubApi.hub();
      this.hubError = null;
    } catch (e) {
      this.hubError = asHubFailure(e);
    }
    return this.hub;
  }

  async loadMachines() {
    try {
      this.machines = await hubApi.machines();
      this.machinesError = null;
    } catch (e) {
      this.machinesError = asHubFailure(e);
    }
    this.now = new Date();
  }

  async loadCodes() {
    try {
      this.codes = await hubApi.joinCodes();
      this.codesError = null;
    } catch (e) {
      this.codesError = asHubFailure(e);
    }
  }

  async createCode(note: string) {
    const code = await hubApi.createJoinCode(note);
    void this.loadCodes();
    return code;
  }

  async revoke(codeId: string) {
    try {
      await hubApi.revokeJoinCode(codeId);
    } catch (e) {
      // Already used, expired or revoked elsewhere: the code is gone either way.
      if (asHubFailure(e).status !== 404) throw e;
    }
    if (this.codes) this.codes = this.codes.filter((c) => c.code_id !== codeId);
    void this.loadCodes();
  }

  /** Rejects with 503 (`pbs_unreachable`) or `pbs_refused`; a 404 counts as removed. */
  async remove(machineId: string): Promise<"removed" | "gone"> {
    let result: "removed" | "gone" = "removed";
    try {
      await hubApi.removeMachine(machineId);
    } catch (e) {
      if (asHubFailure(e).status !== 404) throw e;
      result = "gone";
    }
    if (this.machines) this.machines = { ...this.machines, machines: this.machines.machines.filter((m) => m.machine_id !== machineId) };
    void this.loadMachines();
    return result;
  }
}

export const hub = new HubState();
