// The page's routes, read from the address bar. The server answers every
// path that is not a file or an /api/ endpoint with index.html, so a route
// loads directly and survives a refresh; moving between routes uses the
// History API and never reloads the page.

import { createSignal } from "solid-js";
import type { Interface } from "./api";

export type Route =
  | { page: "ask" }
  | { page: "feed"; interface?: Interface; user?: string }
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

const [current, setCurrent] = createSignal<Route>(parseRoute(new URL(window.location.href)));
window.addEventListener("popstate", () => setCurrent(parseRoute(new URL(window.location.href))));

/** The route the page is on. */
export const route = current;

/** Go to `to` without reloading, as a new history entry. */
export function navigate(to: string): void {
  const url = new URL(to, window.location.href);
  if (url.origin !== window.location.origin) {
    window.location.assign(url);
    return;
  }
  window.history.pushState(null, "", url);
  setCurrent(parseRoute(url));
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
  e.preventDefault();
  navigate(url.pathname + url.search);
}
