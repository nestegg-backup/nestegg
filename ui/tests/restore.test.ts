import { describe, expect, it } from "vitest";
import {
  archiveFolder,
  childPath,
  displayPath,
  folderNameFor,
  folderProblem,
  isWithin,
  restoreRequest,
  selectedArchives,
} from "../src/lib/restore.svelte";

const config = { sources: [{ archive: "home.pxar", path: "/home" }, { archive: "etc.pxar", path: "/etc" }] };

describe("restore helpers", () => {
  it("builds and compares archive paths", () => {
    expect(childPath("/", "alice")).toBe("/alice");
    expect(childPath("/alice", "Dokumente")).toBe("/alice/Dokumente");
    expect(isWithin("/alice/Dokumente", "/alice")).toBe(true);
    expect(isWithin("/alice", "/alice")).toBe(false);
    expect(isWithin("/alicex", "/alice")).toBe(false);
    expect(isWithin("/alice", "/")).toBe(true);
  });

  it("shows paths the way the user knows them", () => {
    expect(displayPath({ archive: "home.pxar", path: "/alice/Dokumente", size: null }, config)).toBe("/home/alice/Dokumente");
    expect(displayPath({ archive: "home.pxar", path: "/", size: null }, config)).toBe("/home");
    expect(displayPath({ archive: "other.pxar", path: "/x", size: null }, config)).toBe("other.pxar:/x");
  });

  it("validates the new folder", () => {
    expect(folderNameFor("2026-11-08T00:37:19Z")).toBe("nestegg-restore-2026-11-08");
    expect(folderProblem("", false)).toBe("empty");
    expect(folderProblem("a/b", false)).toBe("slash");
    expect(folderProblem("..", false)).toBe("invalid");
    expect(folderProblem("nestegg-restore-2026-11-08", false)).toBeNull();
    expect(folderProblem("relative/path", true)).toBe("not_absolute");
    expect(folderProblem("/home/alice/../bob", true)).toBe("invalid");
    expect(folderProblem("/home/alice/restore", true)).toBeNull();
  });
});

describe("restore request", () => {
  const sel = (archive: string, path: string) => ({ archive, path, size: null });

  it("one archive keeps the plain form every daemon understands", () => {
    expect(restoreRequest([sel("home.pxar", "/alice/a"), sel("home.pxar", "/alice/b")], "t", "/home/alice/new")).toEqual({
      snapshot_time: "t",
      archive: "home.pxar",
      paths: ["/alice/a", "/alice/b"],
      target_dir: "/home/alice/new",
    });
  });

  it("an archive whose root is selected restores the whole archive (empty path list)", () => {
    expect(restoreRequest([sel("home.pxar", "/")], "t", "/x").paths).toEqual([]);
  });

  it("several archives go into `archives`, each archive once, grouped in selection order", () => {
    const req = restoreRequest([sel("home.pxar", "/alice/a"), sel("etc.pxar", "/"), sel("home.pxar", "/alice/b")], "t", "/x");
    expect(req).toEqual({
      snapshot_time: "t",
      archives: [
        { archive: "home.pxar", paths: ["/alice/a", "/alice/b"] },
        { archive: "etc.pxar", paths: [] },
      ],
      target_dir: "/x",
    });
    expect("archive" in req).toBe(false);
    expect(selectedArchives([sel("home.pxar", "/a"), sel("etc.pxar", "/"), sel("home.pxar", "/b")])).toEqual(["home.pxar", "etc.pxar"]);
  });

  it("names the subfolder of an archive like the daemon", () => {
    expect(archiveFolder("home.pxar")).toBe("home");
    expect(archiveFolder("usr-local.pxar")).toBe("usr-local");
  });
});
