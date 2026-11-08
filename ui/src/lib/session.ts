// Session token of `nestegg ui` (see ui/README.md, requirement 3).
//
// `nestegg ui` opens `/#token=<token>`. The fragment never reaches a server.
// The page takes the token from there, keeps it in sessionStorage (per tab
// and per origin, so it survives a reload of this tab only), removes it from
// the address bar and sends it as `Authorization: Bearer <token>` with every
// API request. No cookies, no localStorage.

const KEY = "nestegg.session";

export interface SessionEnv {
  location: { hash: string; pathname: string; search: string };
  history: { replaceState(data: unknown, unused: string, url?: string): void };
  /** null when the browser refuses storage (then the token lives in memory only). */
  storage: Pick<Storage, "getItem" | "setItem"> | null;
}

let token: string | null = null;

/** The token from `#token=…`, or null. */
export function tokenFromHash(hash: string): string | null {
  const m = /^#token=([^&]+)/.exec(hash);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return null;
  }
}

/**
 * Take the token from the URL fragment (and remove it from the address bar),
 * or fall back to the one stored for this tab. Returns true if a token was
 * found in the URL.
 */
export function initSession(env: SessionEnv): boolean {
  const fromHash = tokenFromHash(env.location.hash);
  if (fromHash) {
    token = fromHash;
    try {
      env.storage?.setItem(KEY, fromHash);
    } catch {
      // Storage blocked: keep it in memory; a reload then needs the address again.
    }
    env.history.replaceState(null, "", `${env.location.pathname}${env.location.search}`);
    return true;
  }
  try {
    token = env.storage?.getItem(KEY) ?? token;
  } catch {
    // keep the in-memory token
  }
  return false;
}

export function sessionToken(): string | null {
  return token;
}

/** Headers for an API request. */
export function authHeaders(): Record<string, string> {
  return token ? { Authorization: `Bearer ${token}` } : {};
}

function browserEnv(): SessionEnv {
  let storage: SessionEnv["storage"] = null;
  try {
    storage = window.sessionStorage;
  } catch {
    storage = null;
  }
  return { location: window.location, history: window.history, storage };
}

/** Called once at start-up and when the address with a new token is opened in this tab. */
export function initBrowserSession(): boolean {
  return typeof window === "undefined" ? false : initSession(browserEnv());
}
