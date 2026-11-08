// Hash routing: works under any path and with `nestegg ui`'s single-page fallback,
// and keeps relative API URLs (`api/v1/...`) valid.

export type Route =
  | { view: "home" }
  | { view: "self" }
  | { view: "pc"; id: string }
  | { view: "server" }
  | { view: "settings" }
  | { view: "restore"; step: "1" | "2" | "3" | "done" };

export function parseHash(hash: string): Route {
  const parts = hash.replace(/^#\/?/, "").split("/").map((p) => decodeURIComponent(p));
  const [a, b] = parts;
  if (a === "pc" && b === "self") return { view: "self" };
  if (a === "pc" && b) return { view: "pc", id: b };
  if (a === "server") return { view: "server" };
  if (a === "settings") return { view: "settings" };
  if (a === "restore") {
    const step = b === "2" || b === "3" || b === "done" ? b : "1";
    return { view: "restore", step };
  }
  return { view: "home" };
}

export function href(route: Route): string {
  switch (route.view) {
    case "home":
      return "#/";
    case "self":
      return "#/pc/self";
    case "pc":
      return `#/pc/${encodeURIComponent(route.id)}`;
    case "restore":
      return `#/restore/${route.step}`;
    default:
      return `#/${route.view}`;
  }
}

class Router {
  route = $state<Route>(parseHash(typeof location === "undefined" ? "" : location.hash));

  constructor() {
    if (typeof window !== "undefined") {
      window.addEventListener("hashchange", () => {
        this.route = parseHash(location.hash);
      });
    }
  }

  go(route: Route, replace = false) {
    const h = href(route);
    if (replace) {
      history.replaceState(null, "", h);
      this.route = parseHash(h);
    } else {
      location.hash = h;
    }
  }
}

export const router = new Router();
