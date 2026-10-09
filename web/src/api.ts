// Typed client for POST /api/judge. The reply shapes mirror
// crates/api/src/shape.rs (ApiReply, tagged by `kind`).

export interface Pin {
  span: string;
  name: string;
}

export interface CitationView {
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
  | { kind: "rate_limited"; message: string };

/** Ask the judge. Never throws on HTTP-level failures; those come back as
 * an `error` reply so the UI has one rendering path. */
export async function askJudge(
  question: string,
  pins: Pin[],
  sessionId: string,
): Promise<ApiReply> {
  let res: Response;
  try {
    res = await fetch("/api/judge", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ question, session_id: sessionId, pins }),
    });
  } catch {
    return { kind: "error", message: "The judge is unreachable. Is the server running?" };
  }
  const body: unknown = await res.json().catch(() => null);
  if (isReply(body)) return body;
  return {
    kind: "error",
    message: `The judge answered strangely (HTTP ${res.status}). Please try again.`,
  };
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

/** An age as words, as the server's `judge_core::freshness::ago` writes it:
 * `under a minute ago`, `5 minutes ago`, `3 hours ago`, `2 days ago`. */
export function ago(secs: number): string {
  const minute = 60;
  const hour = 60 * minute;
  const day = 24 * hour;
  if (secs < minute) return "under a minute ago";
  const [n, unit] =
    secs < hour
      ? [Math.floor(secs / minute), "minute"]
      : secs < 2 * day
        ? [Math.floor(secs / hour), "hour"]
        : [Math.floor(secs / day), "day"];
  return `${n} ${unit}${n === 1 ? "" : "s"} ago`;
}

/** The footer's data line: `Comprehensive Rules 2026-09-25 · refreshed 3
 * hours ago`, with `· latest refresh failed` when it did; the facts the
 * server could not read are said to be unknown. `elapsed` is the seconds
 * since the server answered, added to its age so the line stays current. */
export function freshnessLine(f: Freshness | null, elapsed = 0): string {
  if (f === null) return "Data freshness unknown";
  const parts = [
    f.cr_version === null
      ? "No Comprehensive Rules loaded"
      : `Comprehensive Rules ${crDate(f.cr_version)}`,
    f.refreshed_secs_ago !== null
      ? `refreshed ${ago(f.refreshed_secs_ago + elapsed)}`
      : f.last_refresh_failed
        ? "no successful refresh yet"
        : "no refresh run recorded yet",
  ];
  if (f.last_refresh_failed) parts.push("latest refresh failed");
  return parts.join(" · ");
}

/** `20260819` → `2026-08-19` (matching the Discord footer). */
export function crDate(v: string): string {
  return /^\d{8}$/.test(v) ? `${v.slice(0, 4)}-${v.slice(4, 6)}-${v.slice(6, 8)}` : v;
}
