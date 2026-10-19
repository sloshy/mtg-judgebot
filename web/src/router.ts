// The page's routes, read from the address bar. The server answers every
// path that is not a file or an /api/ endpoint with index.html, so a route
// loads directly and survives a refresh; moving between routes uses the
// History API and never reloads the page.

import { createSignal } from "solid-js";
import type { Interface } from "./api";

export type Route =
  | { page: "ask" }
  | { page: "feed"; interface?: Interface; user?: string; q?: string }
  | { page: "call"; id: string }
  | { page: "user"; name: string }
  | { page: "missing" };

const INTERFACES: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session", "agent"];

function isInterface(v: string | null): v is Interface {
  return INTERFACES.some((i) => i === v);
}

/** The route `url` names. */
export function parseRoute(url: URL): Route {
  const parts = url.pathname.split("/").filter((p) => p !== "");
  const segment = (i: number) => {
    const raw = parts[i];
    if (raw === undefined) return undefined;
    try {
      return decodeURIComponent(raw);
    } catch {
      return undefined;
    }
  };
  const [first] = parts;
  if (first === undefined) return { page: "ask" };
  const second = segment(1);
  if (parts.length === 1 && first === "feed") {
    const asked = url.searchParams.get("interface");
    return {
      page: "feed",
      interface: isInterface(asked) ? asked : undefined,
      user: url.searchParams.get("user") ?? undefined,
      q: url.searchParams.get("q") || undefined,
    };
  }
  if (parts.length === 2 && first === "calls" && second) return { page: "call", id: second };
  if (parts.length === 2 && first === "users" && second) return { page: "user", name: second };
  return { page: "missing" };
}

/** The address of `route`. */
export function href(route: Route): string {
  switch (route.page) {
    case "ask":
      return "/";
    case "feed": {
      const q = new URLSearchParams();
      if (route.interface) q.set("interface", route.interface);
      if (route.user) q.set("user", route.user);
      if (route.q) q.set("q", route.q);
      const s = q.toString();
      return s === "" ? "/feed" : `/feed?${s}`;
    }
    case "call":
      return `/calls/${encodeURIComponent(route.id)}`;
    case "user":
      return `/users/${encodeURIComponent(route.name)}`;
    case "missing":
      return "/";
  }
}

// Equal routes are one route, so a fragment link (`#main`) moving the
// address bar does not count as a new page and reload what is on it.
const [current, setCurrent] = createSignal<Route>(parseRoute(new URL(window.location.href)), {
  equals: (a, b) => JSON.stringify(a) === JSON.stringify(b),
});
window.addEventListener("popstate", () => setCurrent(parseRoute(new URL(window.location.href))));

/** The route the page is on. */
export const route = current;

// Bumped by every `navigate`, so a click on the route already shown (the
// Feed tab on the feed) reloads it although the route itself is unchanged.
const [visits, setVisits] = createSignal(0);

/** How many times the visitor has navigated: a page that reloads on a
 * repeated click of its own link reads it. */
export const visit = visits;

/** Go to `to` without reloading, as a new history entry. */
export function navigate(to: string): void {
  const url = new URL(to, window.location.href);
  if (url.origin !== window.location.origin) {
    window.location.assign(url);
    return;
  }
  // The same address again is a reload of it, not a new history entry.
  const here = window.location.pathname + window.location.search;
  if (url.pathname + url.search !== here || url.hash !== window.location.hash) {
    window.history.pushState(null, "", url);
  }
  setCurrent(parseRoute(url));
  setVisits((n) => n + 1);
  window.scrollTo(0, 0);
}

/** A click on a link to one of the page's routes, as `navigate`: only a
 * plain left click on a same-origin link, so opening it in a new tab still
 * works. */
export function follow(e: MouseEvent): void {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey)
    return;
  const a = (e.target as Element | null)?.closest("a");
  if (!a || a.target || a.hasAttribute("download")) return;
  const url = new URL(a.href, window.location.href);
  if (url.origin !== window.location.origin || url.pathname.startsWith("/api/")) return;
  // A fragment on this same page (the skip link) is the browser's to scroll to.
  if (
    url.hash !== "" &&
    url.pathname === window.location.pathname &&
    url.search === window.location.search
  )
    return;
  e.preventDefault();
  navigate(url.pathname + url.search);
}
