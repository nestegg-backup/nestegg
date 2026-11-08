import de, { type Messages } from "./de";
import en from "./en";

export type Lang = "de" | "en";
export type { Messages };

export const messages: Record<Lang, Messages> = { de, en };

/** German if the first preferred browser language is German, English otherwise. */
export function pickLang(languages: readonly string[] | undefined): Lang {
  const first = (languages?.[0] ?? "").toLowerCase();
  return first === "de" || first.startsWith("de-") ? "de" : "en";
}

/** Locale for dates and numbers: the first browser locale of the chosen language. */
export function pickLocale(lang: Lang, languages: readonly string[] | undefined): string {
  const match = languages?.find((l) => l.toLowerCase().split("-")[0] === lang);
  return match ?? (lang === "de" ? "de-DE" : "en-GB");
}
