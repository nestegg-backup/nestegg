// State of the restore flow. Kept outside the components so it survives
// moving between the steps.

import { api, ApiFailure } from "./api";
import type { Messages } from "../i18n";
import { isInventoryArchive, restorableArchives } from "./derive";
import type { ConfigView, DirEntry, Restore, RestoreRequest, Snapshot, Target } from "./types";

/** A list of snapshots as it is being loaded (the server's lives in the app store, the stick's here). */
export interface SnapshotList {
  state: "idle" | "loading" | "ok" | "error";
  list: Snapshot[];
  error: ApiFailure | null;
}

export interface TreeNode {
  archive: string;
  /** Path inside the archive; "/" is the archive root. */
  path: string;
  name: string;
  type: DirEntry["type"] | "archive";
  size: number | null;
  children: TreeNode[] | null;
  loading: boolean;
  failed: boolean;
  error: ApiFailure | null;
  open: boolean;
}

export interface Selection {
  archive: string;
  path: string;
  size: number | null;
}

/** Explain refusals without claiming that missing permission metadata means missing files. */
export function browseFailureText(error: ApiFailure | null, t: Messages): string {
  if (error?.status !== 403) return t.restore.dirFailed;
  if (error.reason === "restore_no_home") return t.restore.dirNoHome;
  if (error.reason === "restore_source_unverified") return t.restore.dirNoMetadata;
  if (error.reason === "restore_outside_home") return t.restore.dirOutsideHome;
  return t.restore.dirForbidden(error.message);
}

export function childPath(parent: string, name: string): string {
  return parent === "/" ? `/${name}` : `${parent}/${name}`;
}

export function isWithin(path: string, ancestor: string): boolean {
  return ancestor === "/" ? path !== "/" : path.startsWith(`${ancestor}/`);
}

/** Source folder of an archive (`home.pxar` -> `/home`) from the configuration. */
export function archiveSource(archive: string, config: ConfigView | null): string | null {
  return config?.sources?.find((s) => s.archive === archive)?.path ?? null;
}

/** Path as the user knows it: `/home` + `/alice/x` -> `/home/alice/x`. */
export function displayPath(sel: Selection, config: ConfigView | null): string {
  const source = archiveSource(sel.archive, config);
  if (!source) return sel.path === "/" ? sel.archive : `${sel.archive}:${sel.path}`;
  if (sel.path === "/") return source;
  return `${source.replace(/\/$/, "")}${sel.path}`;
}

/** Archives in the selection, in the order they were first selected. */
export function selectedArchives(selection: Selection[]): string[] {
  return [...new Set(selection.map((s) => s.archive))];
}

/** Subfolder an archive gets when several are restored at once (`home.pxar` -> `home`). */
export function archiveFolder(archive: string): string {
  return archive.replace(/\.pxar$/, "");
}

/**
 * The restore request for a selection. One archive keeps the plain form
 * (`archive` + `paths`, understood by every daemon); several archives use
 * `archives`, and the daemon puts each into its own subfolder. An empty list
 * of paths restores a whole archive (when its root is selected).
 */
export function restoreRequest(
  selection: Selection[],
  snapshotTime: string,
  targetDir: string,
  source: Target = "pbs",
  uuid?: string | null,
): RestoreRequest {
  const groups = selectedArchives(selection).map((archive) => {
    const mine = selection.filter((s) => s.archive === archive);
    return { archive, paths: mine.some((s) => s.path === "/") ? [] : mine.map((s) => s.path) };
  });
  // The server is the default: its request stays exactly as older daemons know it.
  const from = source === "stick" ? { target: "stick" as const, ...(uuid ? { stick_uuid: uuid } : {}) } : {};
  if (groups.length === 1)
    return { ...from, snapshot_time: snapshotTime, archive: groups[0]!.archive, paths: groups[0]!.paths, target_dir: targetDir };
  return { ...from, snapshot_time: snapshotTime, archives: groups, target_dir: targetDir };
}

/** The stick could not be opened or read (503 `stick_unavailable`): not plugged in, pulled out, busy or unreadable. */
export function isStickProblem(e: unknown): e is ApiFailure {
  return e instanceof ApiFailure && (e.kind === "stick_unavailable" || e.code === "stick_unavailable");
}

/**
 * Asked for the stick, the daemon answered as for the server: a version that does not know
 * `target=stick` yet (it ignores the parameter). Its answer is not about the stick.
 */
export function stickNotSupported(e: unknown): boolean {
  return e instanceof ApiFailure && (e.kind === "pbs_unreachable" || e.kind === "not_implemented");
}

export function folderNameFor(snapshotTime: string): string {
  return `nestegg-restore-${snapshotTime.slice(0, 10)}`;
}

export type FolderProblem = "empty" | "slash" | "invalid" | "not_absolute" | null;

export function folderProblem(value: string, fullPath: boolean): FolderProblem {
  const v = value.trim();
  if (!v) return "empty";
  if (fullPath) {
    if (!v.startsWith("/")) return "not_absolute";
    if (v === "/" || v.split("/").some((p) => p === "..")) return "invalid";
    return null;
  }
  if (v.includes("/")) return "slash";
  if (v === "." || v === "..") return "invalid";
  return null;
}

function sortEntries(a: DirEntry, b: DirEntry): number {
  const ad = a.type === "directory" ? 0 : 1;
  const bd = b.type === "directory" ? 0 : 1;
  return ad - bd || a.name.localeCompare(b.name);
}

class RestoreState {
  /** Where the files come from: the backup server or the plugged-in stick. */
  source = $state<Target>("pbs");
  stickUuid = $state<string | null>(null);
  #generation = 0;
  /** Snapshots on the stick; read again whenever the stick is plugged in anew. */
  stickSnaps = $state<SnapshotList>({ state: "idle", list: [], error: null });
  /** The last request to the stick failed (unplugged mid-browse, busy, unreadable); cleared by the next success. */
  stickProblem = $state<ApiFailure | null>(null);
  snapshotTime = $state<string | null>(null);
  showOlder = $state(false);
  /** Result of opening the first archive: tells whether this daemon can browse at all. */
  browse = $state<"unknown" | "ok" | "not_implemented" | "error">("unknown");
  browseError = $state<ApiFailure | null>(null);
  roots = $state<TreeNode[]>([]);
  rootsFor = $state<string | null>(null);
  selection = $state<Selection[]>([]);
  folder = $state("");
  fullPath = $state("");
  starting = $state(false);
  startError = $state<{ exists: boolean; notImplemented: boolean; stick: boolean; message: string } | null>(null);
  job = $state<Restore | null>(null);
  /** Source captured when starting, including legacy PBS jobs without an echo. */
  jobSource = $state<Target>("pbs");
  /** The last poll of the job failed: its current state is unknown. */
  jobUnknown = $state(false);
  jobSelection = $state<Selection[]>([]);
  #poll: ReturnType<typeof setTimeout> | null = null;

  snapshot(list: Snapshot[]): Snapshot | null {
    return list.find((s) => s.time === this.snapshotTime) ?? null;
  }

  /** The snapshot list of the chosen source: the server's (from the app store) or the stick's. */
  snaps(server: SnapshotList): SnapshotList {
    return this.source === "stick" ? this.stickSnaps : server;
  }

  /** A restore from the stick is running in this page: the stick is in use and must stay plugged in. */
  get usingStick(): boolean {
    return this.job?.state === "running" && this.jobSource === "stick";
  }

  /** Switch the source. Snapshots, files and selection belong to one source, so they start over. */
  setSource(source: Target) {
    if (source === this.source) return;
    this.source = source;
    this.#generation++;
    this.stickSnaps = { state: "idle", list: [], error: null };
    this.snapshotTime = null;
    this.showOlder = false;
    this.selection = [];
    this.folder = "";
    this.roots = [];
    this.rootsFor = null;
    this.browse = "unknown";
    this.browseError = null;
    this.stickProblem = null;
    this.startError = null;
  }

  setStick(uuid: string | null) {
    if (uuid === this.stickUuid) return;
    this.stickUuid = uuid;
    this.reset();
  }

  /** Read the snapshots on the stick. Opening the stick takes a moment (it is checked and mounted first). */
  async loadStickSnapshots(force = false) {
    if (!force && this.stickSnaps.state !== "idle") return;
    if (this.stickSnaps.state === "loading") return;
    this.stickSnaps = { state: "loading", list: this.stickSnaps.list, error: null };
    const generation = this.#generation;
    try {
      const response = await api.snapshotSource("stick", this.stickUuid);
      if (generation !== this.#generation) return;
      this.stickUuid = response.stick_uuid!;
      const list = response.snapshots;
      list.sort((a, b) => b.time.localeCompare(a.time));
      this.stickSnaps = { state: "ok", list, error: null };
      this.stickProblem = null;
    } catch (e) {
      if (generation !== this.#generation) return;
      this.stickSnaps = { state: "error", list: [], error: e instanceof ApiFailure ? e : new ApiFailure("other", 0, null, String(e)) };
    }
  }

  /**
   * The stick was pulled out. Its catalog is not kept across plug-ins and another stick may come
   * next, so snapshots and selections are discarded before reading the next attachment.
   */
  /** The stick was pulled out. Its cached snapshot list always goes; the
   *  choices of a restore are dropped only when that restore reads from the
   *  stick, so a restore from the server keeps its snapshot and selection. */
  stickGone() {
    this.stickSnaps = { state: "idle", list: [], error: null };
    if (this.source !== "stick") return;
    this.#generation++;
    this.snapshotTime = null;
    this.selection = [];
    this.roots = [];
    this.rootsFor = null;
  }

  pick(time: string) {
    if (time === this.snapshotTime) return;
    this.#generation++;
    this.snapshotTime = time;
    this.selection = [];
    this.folder = "";
    this.roots = [];
    this.rootsFor = null;
    this.browse = "unknown";
  }

  /** Build the archive roots of the chosen snapshot and open the first one. */
  async prepare(snapshot: Snapshot, config: ConfigView | null) {
    if (this.rootsFor === snapshot.time) return;
    this.rootsFor = snapshot.time;
    const archives = restorableArchives(snapshot.archives);
    const order = (config?.sources ?? []).map((s) => s.archive);
    archives.sort((a, b) => {
      const ia = isInventoryArchive(a) ? 1e6 : order.indexOf(a) === -1 ? 1e5 : order.indexOf(a);
      const ib = isInventoryArchive(b) ? 1e6 : order.indexOf(b) === -1 ? 1e5 : order.indexOf(b);
      return ia - ib || a.localeCompare(b);
    });
    this.roots = archives.map((archive) => ({
      archive,
      path: "/",
      name: archive,
      type: "archive",
      size: null,
      children: null,
      loading: false,
      failed: false,
      error: null,
      open: false,
    }));
    const first = this.roots[0];
    if (first) await this.toggle(first);
    else this.browse = "ok";
  }

  async toggle(node: TreeNode) {
    if (node.open) {
      node.open = false;
      return;
    }
    node.open = true;
    if (node.children || node.loading) return;
    node.loading = true;
    node.failed = false;
    node.error = null;
    const generation = this.#generation;
    try {
      const entries = await api.browse(this.rootsFor!, node.archive, node.path, this.source, this.stickUuid);
      if (generation !== this.#generation) return;
      if (this.source === "stick") this.stickProblem = null;
      entries.sort(sortEntries);
      node.children = entries.map((e) => ({
        archive: node.archive,
        path: childPath(node.path, e.name),
        name: e.name,
        type: e.type,
        size: e.size_bytes ?? null,
        children: null,
        loading: false,
        failed: false,
        error: null,
        open: false,
      }));
      if (this.browse === "unknown") this.browse = "ok";
    } catch (e) {
      if (generation !== this.#generation) return;
      node.failed = true;
      node.error = e instanceof ApiFailure ? e : null;
      node.open = false;
      if (this.source === "stick" && isStickProblem(e)) this.stickProblem = e;
      if (e instanceof ApiFailure && e.kind === "not_implemented") this.browse = "not_implemented";
      else if (this.browse === "unknown") {
        this.browse = "error";
        this.browseError = e instanceof ApiFailure ? e : null;
      }
    } finally {
      node.loading = false;
    }
  }

  /** Selected explicitly or through a selected parent folder. */
  inherited(node: TreeNode): boolean {
    return this.selection.some((s) => s.archive === node.archive && isWithin(node.path, s.path));
  }

  isSelected(node: TreeNode): boolean {
    return this.selection.some((s) => s.archive === node.archive && s.path === node.path);
  }

  setSelected(node: TreeNode, on: boolean) {
    if (on) {
      this.selection = [
        ...this.selection.filter((s) => !(s.archive === node.archive && isWithin(s.path, node.path))),
        { archive: node.archive, path: node.path, size: node.type === "file" ? node.size : null },
      ];
    } else {
      this.selection = this.selection.filter((s) => !(s.archive === node.archive && s.path === node.path));
    }
  }

  /** Total size if every selected item has a known size (files), else null. */
  totalSize(): number | null {
    if (!this.selection.length || this.selection.some((s) => s.size == null)) return null;
    return this.selection.reduce((a, s) => a + (s.size ?? 0), 0);
  }

  targetDir(home: string | null): string {
    return home ? `${home.replace(/\/$/, "")}/${this.folder.trim()}` : this.fullPath.trim();
  }

  async start(home: string | null): Promise<boolean> {
    if (!this.selection.length || !this.snapshotTime) return false;
    const source = this.source;
    const selection = [...this.selection];
    this.starting = true;
    this.startError = null;
    try {
      this.job = await api.startRestore(restoreRequest(this.selection, this.snapshotTime, this.targetDir(home), this.source, this.stickUuid));
      this.jobSource = source;
      this.jobSelection = selection;
      this.watch();
      return true;
    } catch (e) {
      const f = e instanceof ApiFailure ? e : null;
      const exists = !!f && (f.code === "target_exists" || (f.kind === "conflict" && /exist/i.test(f.message)));
      const stick = source === "stick" && isStickProblem(e);
      if (stick) this.stickProblem = f;
      this.startError = { exists, notImplemented: f?.kind === "not_implemented", stick, message: f?.message ?? String(e) };
      return false;
    } finally {
      this.starting = false;
    }
  }

  watch(delay = 1_500) {
    if (this.#poll) clearTimeout(this.#poll);
    const job = this.job;
    if (!job || job.state !== "running") return;
    this.#poll = setTimeout(async () => {
      try {
        const updated = await api.restore(job.id, this.jobSource, job.stick_uuid);
        if (this.job?.id !== job.id) return;
        this.job = updated;
        this.jobUnknown = false;
      } catch {
        // Keep the last known state, but say that it is unknown now, and try again.
        this.jobUnknown = true;
      }
      this.watch();
    }, delay);
  }

  reset() {
    this.#generation++;
    if (this.#poll) clearTimeout(this.#poll);
    // The source stays: it is chosen again on the first step, or fixed when there is only the stick.
    this.stickProblem = null;
    this.stickSnaps = { state: "idle", list: [], error: null };
    this.snapshotTime = null;
    this.showOlder = false;
    this.browse = "unknown";
    this.browseError = null;
    this.roots = [];
    this.rootsFor = null;
    this.selection = [];
    this.folder = "";
    this.fullPath = "";
    this.startError = null;
    this.job = null;
    this.jobUnknown = false;
    this.jobSelection = [];
  }
}

export const restore = new RestoreState();
