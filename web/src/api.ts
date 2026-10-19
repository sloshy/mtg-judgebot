// Typed client for the HTTP API. The reply shapes mirror
// crates/api/src/shape.rs (ApiReply, tagged by `kind`) and
// crates/api/src/feed.rs (the feed's pages and entries).

import type { Locale, Messages } from "./messages";

export interface Pin {
  span: string;
  name: string;
}

/** What a citation quotes (crates/api/src/shape.rs). */
export type CitationKind = "rule" | "ruling" | "oracle_text" | "prior_call";

export interface CitationView {
  /** Absent from an older server. */
  kind?: CitationKind;
  label: string;
  url: string | null;
  quote: string;
}

export interface CardView {
  name: string;
  url: string;
}

export interface AmbiguousView {
  query: string;
  choices: string[];
  truncated: boolean;
}

export type ApiReply =
  | {
      kind: "answer";
      answer: string;
      confidence: "low" | "medium" | "high";
      source: "cr" | "commander";
      cr_version: string;
      citations: CitationView[];
      cards: CardView[];
    }
  | { kind: "ambiguous"; spans: AmbiguousView[] }
  | { kind: "not_found"; names: string[] }
  | { kind: "error"; message: string }
  | { kind: "busy"; message: string }
  | { kind: "rate_limited"; message: string }
  | { kind: "maintenance"; message: string; retry_after_secs: number };

/** Ask the judge, asking for its replies in `locale` (none: the server
 * chooses, from `Accept-Language` then its default). Never throws on
 * HTTP-level failures; those come back as an `error` reply (in `m`) so the
 * UI has one rendering path. */
export async function askJudge(
  question: string,
  pins: Pin[],
  sessionId: string,
  locale: Locale | undefined,
  m: Messages,
): Promise<ApiReply> {
  let res: Response;
  try {
    res = await fetch("/api/judge", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ question, session_id: sessionId, pins, locale }),
    });
  } catch {
    return { kind: "error", message: m.unreachable };
  }
  const body: unknown = await res.json().catch(() => null);
  if (isReply(body)) return body;
  return { kind: "error", message: m.strangeReply(res.status) };
}

function isReply(v: unknown): v is ApiReply {
  return typeof v === "object" && v !== null && typeof (v as { kind?: unknown }).kind === "string";
}

/** GET /api/about: the source offer and the operator's contact
 * (crates/core/src/source.rs `About`). */
export interface About {
  program: string;
  repository: string;
  commit: string | null;
  commit_url: string | null;
  dirty: boolean;
  license: string;
  license_name: string;
  license_url: string;
  copyright: string;
  /** Who runs this instance. The server refuses to start without the
   * address; the Discord username is there when the operator set it too. */
  operator_discord: string | null;
  operator_email: string | null;
  notice: string;
  /** What the data is (crates/core/src/freshness.rs `Freshness`); null when
   * the server could not read it. */
  freshness: Freshness | null;
  /** The instance's default locale (`JUDGE_LOCALE`), and the locales the
   * server can reply in. Absent from an older server. */
  locale?: string;
  locales?: string[];
  /** Whether the public feed is served here. Absent from an older server. */
  feed?: boolean;
}

/** The Comprehensive Rules release loaded and the last refresh. */
export interface Freshness {
  /** `YYYYMMDD`; null when no rules are loaded. */
  cr_version: string | null;
  /** Seconds since the latest successful refresh finished, on the server's
   * clock when it answered; null when none is recorded. */
  refreshed_secs_ago: number | null;
  /** Whether the latest refresh failed. */
  last_refresh_failed: boolean;
}

/** The source offer, or null when the server did not answer: the footer
 * then falls back to the upstream repository so the page never shows
 * nothing at all. */
export async function fetchAbout(): Promise<About | null> {
  try {
    const res = await fetch("/api/about");
    if (!res.ok) return null;
    const body: unknown = await res.json();
    return isAbout(body) ? body : null;
  } catch {
    return null;
  }
}

function isAbout(v: unknown): v is About {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.repository === "string" &&
    typeof o.notice === "string" &&
    (o.freshness === null || isFreshness(o.freshness))
  );
}

function isFreshness(v: unknown): v is Freshness {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    (o.cr_version === null || typeof o.cr_version === "string") &&
    (o.refreshed_secs_ago === null || typeof o.refreshed_secs_ago === "number") &&
    typeof o.last_refresh_failed === "boolean"
  );
}

/** The session id questions share history under; kept per browser tab. */
export function sessionId(): string {
  const key = "judgebot-session";
  try {
    const existing = sessionStorage.getItem(key);
    if (existing) return existing;
    const fresh = crypto.randomUUID();
    sessionStorage.setItem(key, fresh);
    return fresh;
  } catch {
    return crypto.randomUUID();
  }
}

/** The footer's data line: `Comprehensive Rules 2026-09-25 · refreshed 3
 * hours ago.`, with `· latest refresh failed` when it did; the facts the
 * server could not read are said to be unknown. `elapsed` is the seconds
 * since the server answered, added to its age so the line stays current. */
export function freshnessLine(f: Freshness | null, m: Messages, elapsed = 0): string {
  if (f === null) return m.freshnessLine([m.freshnessUnknown]);
  const parts = [
    f.cr_version === null ? m.noRules : m.rulesVersion(m.crDate(f.cr_version)),
    f.refreshed_secs_ago !== null
      ? m.refreshed(f.refreshed_secs_ago + elapsed)
      : f.last_refresh_failed
        ? m.noSuccessfulRefresh
        : m.noRefreshRecorded,
  ];
  if (f.last_refresh_failed) parts.push(m.refreshFailed);
  return m.freshnessLine(parts);
}

/** Where a feed entry was asked (crates/bot/src/feed.rs `Asked`). */
export type Interface = "discord" | "web" | "mcp" | "cli" | "agent_session" | "agent";

/** One public call (crates/api/src/feed.rs `EntryView`). */
export interface FeedEntry {
  id: string;
  /** RFC 3339 UTC. */
  asked_at: string;
  interface: Interface;
  /** The Discord username; null is anonymous. */
  asker: string | null;
  question: string;
  answer: string;
  confidence: "low" | "medium" | "high";
  source: "cr" | "commander";
  cr_version: string;
  citations: CitationView[];
  cards: CardView[];
  ratings: {
    correct: number;
    partial: number;
    incorrect: number;
    judge: "correct" | "partial" | "incorrect" | null;
  };
  /** Why it no longer serves as an example; null while its citations hold. */
  retired: string | null;
}

/** A page of the feed: newest first, `next` the cursor of the one after. */
export interface FeedPage {
  entries: FeedEntry[];
  next: string | null;
}

/** A read of the feed: the value, or the server's message (in `m` when it
 * sent none). */
export type Read<T> = { ok: true; value: T } | { ok: false; status: number; message: string };

/** Which calls a feed page holds. */
export interface FeedQuery {
  cursor?: string;
  interface?: Interface;
  user?: string;
  /** Text the question, answer or asker's username contains. */
  q?: string;
}

async function read<T>(path: string, m: Messages, valid: (v: unknown) => v is T): Promise<Read<T>> {
  let res: Response;
  try {
    res = await fetch(path);
  } catch {
    return { ok: false, status: 0, message: m.unreachable };
  }
  const body: unknown = await res.json().catch(() => null);
  if (res.ok && valid(body)) return { ok: true, value: body };
  const message =
    typeof body === "object" &&
    body !== null &&
    typeof (body as { message?: unknown }).message === "string"
      ? (body as { message: string }).message
      : m.strangeReply(res.status);
  return { ok: false, status: res.status, message };
}

function withLocale(params: URLSearchParams, locale: Locale | undefined): string {
  if (locale !== undefined) params.set("locale", locale);
  const q = params.toString();
  return q === "" ? "" : `?${q}`;
}

/** GET /api/feed. */
export function fetchFeed(
  q: FeedQuery,
  locale: Locale | undefined,
  m: Messages,
): Promise<Read<FeedPage>> {
  const params = new URLSearchParams();
  if (q.cursor) params.set("cursor", q.cursor);
  if (q.interface) params.set("interface", q.interface);
  if (q.user) params.set("user", q.user);
  if (q.q) params.set("q", q.q);
  return read(`/api/feed${withLocale(params, locale)}`, m, isFeedPage);
}

/** GET /api/calls/{id}. */
export function fetchCall(
  id: string,
  locale: Locale | undefined,
  m: Messages,
): Promise<Read<FeedEntry>> {
  return read(
    `/api/calls/${encodeURIComponent(id)}${withLocale(new URLSearchParams(), locale)}`,
    m,
    isEntry,
  );
}

function isFeedPage(v: unknown): v is FeedPage {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    Array.isArray(o.entries) &&
    o.entries.every(isEntry) &&
    (o.next === null || typeof o.next === "string")
  );
}

function isEntry(v: unknown): v is FeedEntry {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    typeof o.question === "string" &&
    typeof o.answer === "string" &&
    Array.isArray(o.citations) &&
    Array.isArray(o.cards) &&
    typeof o.ratings === "object" &&
    o.ratings !== null
  );
}
