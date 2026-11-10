// Theme choice of the title bar: follow the system, or always light/dark.
//
// Kept per browser in localStorage (the only thing this UI stores there; the
// session token lives in sessionStorage, see session.ts). Every access is
// wrapped: with blocked storage the choice lasts until the page is closed.
// public/theme.js applies the stored choice before the first paint.

export type ThemeChoice = "system" | "light" | "dark";

export const THEME_KEY = "nestegg.theme";
export const THEME_CHOICES: readonly ThemeChoice[] = ["system", "light", "dark"];

type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;

function storage(): Store | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

export function parseTheme(value: string | null | undefined): ThemeChoice {
  return value === "light" || value === "dark" ? value : "system";
}

export function readTheme(store: Store | null = storage()): ThemeChoice {
  try {
    return parseTheme(store?.getItem(THEME_KEY));
  } catch {
    return "system";
  }
}

export function storeTheme(choice: ThemeChoice, store: Store | null = storage()): void {
  try {
    if (choice === "system") store?.removeItem(THEME_KEY);
    else store?.setItem(THEME_KEY, choice);
  } catch {
    // Storage blocked: the choice applies to this page only.
  }
}

/** Set or remove `data-theme` on <html>; tokens.css does the rest. */
export function applyTheme(choice: ThemeChoice, root: { setAttribute(n: string, v: string): void; removeAttribute(n: string): void }): void {
  if (choice === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", choice);
}
