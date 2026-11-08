import { describe, expect, it } from "vitest";
import de from "../src/i18n/de";
import en from "../src/i18n/en";
import { pickLang, pickLocale } from "../src/i18n";

/** All leaf paths with the type of their value. */
function shape(obj: unknown, prefix = ""): Record<string, string> {
  const out: Record<string, string> = {};
  if (Array.isArray(obj)) {
    out[prefix] = `array(${obj.length})`;
    return out;
  }
  if (obj && typeof obj === "object") {
    for (const [k, v] of Object.entries(obj)) Object.assign(out, shape(v, prefix ? `${prefix}.${k}` : k));
    return out;
  }
  out[prefix] = typeof obj;
  return out;
}

describe("translations", () => {
  it("German and English have identical keys and value types", () => {
    expect(shape(en)).toEqual(shape(de));
  });

  it("functions take the same number of parameters", () => {
    const walk = (a: unknown, b: unknown, path: string) => {
      if (typeof a === "function") {
        expect((b as (...x: unknown[]) => unknown).length, path).toBe((a as (...x: unknown[]) => unknown).length);
      } else if (a && typeof a === "object" && !Array.isArray(a)) {
        for (const k of Object.keys(a)) walk((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k], `${path}.${k}`);
      }
    };
    walk(de, en, "");
  });

  it("has no empty strings", () => {
    for (const [lang, msgs] of Object.entries({ de, en })) {
      const walk = (v: unknown, path: string) => {
        if (typeof v === "string") expect(v.trim(), `${lang}${path}`).not.toBe("");
        else if (v && typeof v === "object") for (const [k, x] of Object.entries(v)) walk(x, `${path}.${k}`);
      };
      walk(msgs, "");
    }
  });
});

describe("language choice", () => {
  it("follows the first browser language", () => {
    expect(pickLang(["de-DE", "en"])).toBe("de");
    expect(pickLang(["de"])).toBe("de");
    expect(pickLang(["de-AT"])).toBe("de");
    expect(pickLang(["en-US", "de-DE"])).toBe("en");
    expect(pickLang(["fr-FR"])).toBe("en");
    expect(pickLang(["dev"])).toBe("en");
    expect(pickLang([])).toBe("en");
    expect(pickLang(undefined)).toBe("en");
  });

  it("uses a matching browser locale for numbers and dates", () => {
    expect(pickLocale("de", ["de-AT", "en"])).toBe("de-AT");
    expect(pickLocale("en", ["en-US"])).toBe("en-US");
    expect(pickLocale("en", ["fr-FR"])).toBe("en-GB");
  });
});
