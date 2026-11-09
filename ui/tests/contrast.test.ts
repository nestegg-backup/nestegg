// WCAG AA check of the colour tokens in both themes.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("../src/styles/tokens.css", import.meta.url), "utf8");

function tokens(block: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of block.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})\s*;/gi)) out[m[1]!] = m[2]!.toLowerCase();
  return out;
}

/** The body of the first rule that starts with `selector {`. */
function block(selector: string): string {
  const start = css.indexOf(`${selector} {`);
  if (start < 0) throw new Error(`no rule ${selector}`);
  return css.slice(start, css.indexOf("}", start));
}

const dark = tokens(block(":root"));
const lightSystem = tokens(block(':root:not([data-theme="dark"])'));
const lightChosen = tokens(block(':root[data-theme="light"]'));

function lum(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0]! + 0.7152 * c[1]! + 0.0722 * c[2]!;
}
function ratio(a: string, b: string): number {
  const [x, y] = [lum(a), lum(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

describe("token sets", () => {
  it("the light theme is the same whether chosen or taken from the system", () => {
    expect(lightSystem).toEqual(lightChosen);
  });

  it("both themes define the same colour tokens", () => {
    expect(Object.keys(lightChosen).sort()).toEqual(Object.keys(dark).sort());
  });
});

describe.each([
  ["dark", dark],
  ["light", lightChosen],
])("%s theme", (_name, t) => {
  const need = (fg: string, bgs: string[], min: number) => {
    for (const bg of bgs) {
      expect(t[fg], fg).toBeDefined();
      expect(t[bg], bg).toBeDefined();
      expect(ratio(t[fg]!, t[bg]!), `${fg} on ${bg}`).toBeGreaterThanOrEqual(min);
    }
  };

  it("body text reaches 4.5:1 on every surface it is used on", () => {
    need("text", ["win", "side", "box", "box-hover", "field", "sel", "err-bg"], 4.5);
    need("muted", ["win", "side", "box", "field", "sel", "err-bg"], 4.5);
  });

  // `faint` labels (table heads, group titles, fact labels) never sit on a
  // selected row; on `box` it is the disabled-button text (exempt, but met).
  it("faint text reaches 4.5:1 on the surfaces it is used on", () => {
    need("faint", ["win", "side", "box"], 4.5);
  });

  it("links and the back control reach 4.5:1", () => {
    need("accent-text", ["win", "side", "box"], 4.5);
  });

  it("status colours used as text reach 4.5:1", () => {
    need("bad", ["win", "side", "box", "sel", "err-bg"], 4.5);
    need("warn", ["win", "side", "sel"], 4.5);
    need("ok", ["win", "box"], 4.5);
    need("pbs", ["win", "side"], 4.5);
  });

  it("text on filled marks and buttons reaches 4.5:1", () => {
    need("on-accent", ["accent", "accent-hover"], 4.5);
    need("on-tick", ["tick"], 4.5); // day number in a backed-up day
    need("on-bad-fill", ["bad-fill"], 4.5); // day number in a failed day
    need("on-pbs", ["pbs"], 4.5); // day number in a "server not reachable" day
  });

  it("non-text marks, field borders and switches reach 3:1", () => {
    need("tick", ["win", "side"], 3);
    need("field-border", ["win", "box", "field"], 3);
    need("bad-fill", ["win"], 3);
    need("pbs", ["win", "side"], 3);
    need("bad", ["side", "sel"], 3);
    need("warn", ["side", "sel"], 3);
    // A switch shows its state by the knob: white knob on the accent fill
    // (on, checked above with 4.5:1) or the muted knob in a bordered field (off).
    need("muted", ["field"], 3);
    need("on-accent", ["accent"], 3);
  });
});
