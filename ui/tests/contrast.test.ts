// WCAG AA check of the colour tokens in both themes.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("../src/styles/tokens.css", import.meta.url), "utf8");

function tokens(block: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of block.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})\s*;/gi)) out[m[1]!] = m[2]!.toLowerCase();
  return out;
}

const darkBlock = css.slice(0, css.indexOf("@media (prefers-color-scheme: light)"));
const lightBlock = css.slice(css.indexOf("@media (prefers-color-scheme: light)"));
const dark = tokens(darkBlock);
const light = { ...dark, ...tokens(lightBlock) };

function lum(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0]! + 0.7152 * c[1]! + 0.0722 * c[2]!;
}
function ratio(a: string, b: string): number {
  const [x, y] = [lum(a), lum(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

describe.each([
  ["dark", dark],
  ["light", light],
])("%s theme", (_name, t) => {
  const surfaces = ["bg0", "bg1", "bg2"];

  it.each(["text", "muted", "accent"])("text colour %s reaches 4.5:1 on every surface", (fg) => {
    for (const bg of surfaces) expect(ratio(t[fg]!, t[bg]!), `${fg} on ${bg}`).toBeGreaterThanOrEqual(4.5);
  });

  // `faint` text sits on bg0/bg1 only; on bg2 it is used for disabled buttons,
  // which WCAG 1.4.3 exempts.
  it("faint text reaches 4.5:1 on the page and the list pane", () => {
    for (const bg of ["bg0", "bg1"]) expect(ratio(t.faint!, t[bg]!), `faint on ${bg}`).toBeGreaterThanOrEqual(4.5);
  });

  it("status colours used as text reach 4.5:1 on the page", () => {
    for (const fg of ["bad", "warn", "ok", "pbs"]) expect(ratio(t[fg]!, t.bg0!), fg).toBeGreaterThanOrEqual(4.5);
  });

  it("primary button text reaches 4.5:1", () => {
    expect(ratio(t["on-accent"]!, t.accent!)).toBeGreaterThanOrEqual(4.5);
    expect(ratio(t["on-accent"]!, t["accent-hover"]!)).toBeGreaterThanOrEqual(4.5);
  });

  it("non-text marks and field borders reach 3:1", () => {
    expect(ratio(t.mark!, t.bg0!)).toBeGreaterThanOrEqual(3);
    // Input fields sit on the page; their border marks the boundary (WCAG 1.4.11).
    expect(ratio(t["field-border"]!, t.bg0!)).toBeGreaterThanOrEqual(3);
    for (const s of ["ok", "warn", "bad", "pbs"]) expect(ratio(t[s]!, t.bg1!), s).toBeGreaterThanOrEqual(3);
  });
});
