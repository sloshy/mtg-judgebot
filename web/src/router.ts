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
  | { page: "admin"; view: AdminView }
  | { page: "missing" };

/** Which stored calls the admin list shows (GET /admin/api/calls). */
export interface AdminCallFilter {
  interface?: Interface;
  /** A Discord username or user id. */
  asker?: string;
  model?: string;
  /** true: only private calls; false: only public ones. */
  private?: boolean;
  q?: string;
}

/** A feedback item's state (`judge_core::QueueStatus`). */
export type QueueStatus = "open" | "resolved" | "dismissed";
/** What kind of problem a rater found (`judge_core::FeedbackCategory`). */
export type FeedbackCategory =
  | "wrong_conclusion"
  | "missed_part"
  | "bad_citation"
  | "wrong_card"
  | "misread_question"
  | "wrong_refusal"
  | "outdated"
  | "other";
/** Who rated (`judge_core::RaterKind`). */
export type RaterKind = "discord" | "web";

export const QUEUE_STATUSES: readonly QueueStatus[] = ["open", "resolved", "dismissed"];
export const FEEDBACK_CATEGORIES: readonly FeedbackCategory[] = [
  "wrong_conclusion",
  "missed_part",
  "bad_citation",
  "wrong_card",
  "misread_question",
  "wrong_refusal",
  "outdated",
  "other",
];
export const RATER_KINDS: readonly RaterKind[] = ["discord", "web"];

/** Which items the feedback queue shows (GET /admin/api/queue). */
export interface AdminQueueFilter {
  status?: QueueStatus;
  category?: FeedbackCategory;
  interface?: Interface;
  rater?: RaterKind;
}

/** The admin pages, under /admin (served by the --admin listener only). */
export type AdminView =
  | { view: "calls"; filter: AdminCallFilter }
  | { view: "call"; id: string }
  | { view: "feedback"; filter: AdminQueueFilter }
  | { view: "item"; id: string }
  | { view: "failures" }
  | { view: "usage"; days?: number }
  | { view: "missing" };

const INTERFACES: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session", "agent"];

function isInterface(v: string | null): v is Interface {
  return INTERFACES.some((i) => i === v);
}

function oneOf<T extends string>(all: readonly T[], v: string | null): T | undefined {
  return all.find((x) => x === v);
}

/** The admin view `parts` (the path after /admin) and `params` name. */
function parseAdmin(parts: (string | undefined)[], params: URLSearchParams): AdminView {
  const [first, second] = parts;
  if (first === undefined) {
    const asked = params.get("interface");
    const visibility = params.get("private");
    return {
      view: "calls",
      filter: {
        interface: isInterface(asked) ? asked : undefined,
        asker: params.get("asker") || undefined,
        model: params.get("model") || undefined,
        private: visibility === "true" ? true : visibility === "false" ? false : undefined,
        q: params.get("q") || undefined,
      },
    };
  }
  if (parts.length === 2 && first === "calls" && second) return { view: "call", id: second };
  if (parts.length === 1 && first === "feedback") {
    return {
      view: "feedback",
      filter: {
        status: oneOf(QUEUE_STATUSES, params.get("status")),
        category: oneOf(FEEDBACK_CATEGORIES, params.get("category")),
        interface: oneOf(INTERFACES, params.get("interface")),
        rater: oneOf(RATER_KINDS, params.get("rater")),
      },
    };
  }
  if (parts.length === 2 && first === "feedback" && second) return { view: "item", id: second };
  if (parts.length === 1 && first === "failures") return { view: "failures" };
  if (parts.length === 1 && first === "usage") {
    const days = Number.parseInt(params.get("days") ?? "", 10);
    return { view: "usage", days: Number.isFinite(days) && days > 0 ? days : undefined };
  }
  return { view: "missing" };
}

/** The address of the admin view `v`. */
function adminHref(v: AdminView): string {
  switch (v.view) {
    case "calls": {
      const q = new URLSearchParams();
      if (v.filter.interface) q.set("interface", v.filter.interface);
      if (v.filter.asker) q.set("asker", v.filter.asker);
      if (v.filter.model) q.set("model", v.filter.model);
      if (v.filter.private !== undefined) q.set("private", String(v.filter.private));
      if (v.filter.q) q.set("q", v.filter.q);
      const s = q.toString();
      return s === "" ? "/admin" : `/admin?${s}`;
    }
    case "call":
      return `/admin/calls/${encodeURIComponent(v.id)}`;
    case "feedback": {
      const q = new URLSearchParams();
      if (v.filter.status) q.set("status", v.filter.status);
      if (v.filter.category) q.set("category", v.filter.category);
      if (v.filter.interface) q.set("interface", v.filter.interface);
      if (v.filter.rater) q.set("rater", v.filter.rater);
      const s = q.toString();
      return s === "" ? "/admin/feedback" : `/admin/feedback?${s}`;
    }
    case "item":
      return `/admin/feedback/${encodeURIComponent(v.id)}`;
    case "failures":
      return "/admin/failures";
    case "usage":
      return v.days === undefined ? "/admin/usage" : `/admin/usage?days=${v.days}`;
    case "missing":
      return "/admin";
  }
}

/** Whether `pathname` is the admin pages' own: exactly `/admin`, or under
 * `/admin/`. The server serves them there on the --admin listener only. */
export function isAdminPath(pathname: string): boolean {
  return pathname === "/admin" || pathname.startsWith("/admin/");
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
  // The admin pages only at exactly /admin or under /admin/: no variant
  // spelling (`//admin`, which the split above would fold) routes there.
  if (isAdminPath(url.pathname)) {
    return {
      page: "admin",
      view: parseAdmin(
        parts.slice(1).map((_, i) => segment(i + 1)),
        url.searchParams,
      ),
    };
  }
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
    case "admin":
      return adminHref(route.view);
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
  if (
    url.origin !== window.location.origin ||
    url.pathname.startsWith("/api/") ||
    url.pathname.startsWith("/admin/api/")
  )
    return;
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
