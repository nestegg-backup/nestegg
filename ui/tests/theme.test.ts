// Theme choice of the title bar (src/lib/theme.ts, public/theme.js).
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { applyTheme, parseTheme, readTheme, storeTheme, THEME_KEY } from "../src/lib/theme";

function memory() {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
}

const blocked = {
  getItem: (): string | null => {
    throw new Error("SecurityError");
  },
  setItem: () => {
    throw new Error("SecurityError");
  },
  removeItem: () => {
    throw new Error("SecurityError");
  },
};

describe("theme choice", () => {
  it("accepts only light and dark, everything else follows the system", () => {
    expect(parseTheme("light")).toBe("light");
    expect(parseTheme("dark")).toBe("dark");
    for (const v of [null, undefined, "", "system", "Dark", "blue"]) expect(parseTheme(v)).toBe("system");
  });

  it("stores light/dark and forgets the key for system", () => {
    const s = memory();
    storeTheme("dark", s);
    expect(s.data.get(THEME_KEY)).toBe("dark");
    expect(readTheme(s)).toBe("dark");
    storeTheme("system", s);
    expect(s.data.has(THEME_KEY)).toBe(false);
    expect(readTheme(s)).toBe("system");
  });

  it("works without storage", () => {
    expect(readTheme(null)).toBe("system");
    expect(readTheme(blocked)).toBe("system");
    expect(() => storeTheme("light", blocked)).not.toThrow();
    expect(() => storeTheme("light", null)).not.toThrow();
  });

  it("sets data-theme on the root only for an explicit choice", () => {
    const attrs = new Map<string, string>();
    const root = { setAttribute: (n: string, v: string) => void attrs.set(n, v), removeAttribute: (n: string) => void attrs.delete(n) };
    applyTheme("light", root);
    expect(attrs.get("data-theme")).toBe("light");
    applyTheme("system", root);
    expect(attrs.has("data-theme")).toBe(false);
  });

  it("the pre-paint script reads the same key and loads before the app", () => {
    const script = readFileSync(new URL("../public/theme.js", import.meta.url), "utf8");
    expect(script).toContain(`"${THEME_KEY}"`);
    const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
    expect(html.indexOf('src="./theme.js"')).toBeGreaterThan(0);
    expect(html.indexOf('src="./theme.js"')).toBeLessThan(html.indexOf("/src/main.ts"));
  });
});
