// State of the restore flow. Kept outside the components so it survives
// moving between the steps.

import { api, ApiFailure } from "./api";
import { isInventoryArchive, restorableArchives } from "./derive";
import type { ConfigView, DirEntry, Restore, RestoreRequest, Snapshot } from "./types";

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
  open: boolean;
}

export interface Selection {
  archive: string;
  path: string;
  size: number | null;
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
export function restoreRequest(selection: Selection[], snapshotTime: string, targetDir: string): RestoreRequest {
  const groups = selectedArchives(selection).map((archive) => {
    const mine = selection.filter((s) => s.archive === archive);
    return { archive, paths: mine.some((s) => s.path === "/") ? [] : mine.map((s) => s.path) };
  });
  if (groups.length === 1) return { snapshot_time: snapshotTime, archive: groups[0]!.archive, paths: groups[0]!.paths, target_dir: targetDir };
  return { snapshot_time: snapshotTime, archives: groups, target_dir: targetDir };
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
  startError = $state<{ exists: boolean; notImplemented: boolean; message: string } | null>(null);
  job = $state<Restore | null>(null);
  /** The last poll of the job failed: its current state is unknown. */
  jobUnknown = $state(false);
  jobSelection = $state<Selection[]>([]);
  #poll: ReturnType<typeof setTimeout> | null = null;

  snapshot(list: Snapshot[]): Snapshot | null {
    return list.find((s) => s.time === this.snapshotTime) ?? null;
  }

  pick(time: string) {
    if (time === this.snapshotTime) return;
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
    try {
      const entries = await api.browse(this.rootsFor!, node.archive, node.path);
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
        open: false,
      }));
      if (this.browse === "unknown") this.browse = "ok";
    } catch (e) {
      node.failed = true;
      node.open = false;
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
    this.starting = true;
    this.startError = null;
    try {
      this.job = await api.startRestore(restoreRequest(this.selection, this.snapshotTime, this.targetDir(home)));
      this.jobSelection = [...this.selection];
      this.watch();
      return true;
    } catch (e) {
      const f = e instanceof ApiFailure ? e : null;
      const exists = !!f && (f.code === "target_exists" || (f.kind === "conflict" && /exist/i.test(f.message)));
      this.startError = { exists, notImplemented: f?.kind === "not_implemented", message: f?.message ?? String(e) };
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
        this.job = await api.restore(job.id);
        this.jobUnknown = false;
      } catch {
        // Keep the last known state, but say that it is unknown now, and try again.
        this.jobUnknown = true;
      }
      this.watch();
    }, delay);
  }

  reset() {
    if (this.#poll) clearTimeout(this.#poll);
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
