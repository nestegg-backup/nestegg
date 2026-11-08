import { describe, expect, it } from "vitest";
import { childPath, displayPath, folderNameFor, folderProblem, isWithin } from "../src/lib/restore.svelte";

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
