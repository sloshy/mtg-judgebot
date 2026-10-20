// The admin pages' client for /admin/api/* (crates/api/src/admin.rs), and
// the token those calls carry.
//
// The token arrives in the address's fragment (`/admin#token=…`, printed by
// `judge-cli admin-url`), which a browser never sends to a server. It is
// taken from there once, removed from the address bar (and so from the
// history and any copied link), and kept in this tab's sessionStorage. It
// travels only as an `Authorization: Bearer` header, never in a query.

import type { CitationView, Interface } from "../api";
import type {
  AdminCallFilter,
  AdminQueueFilter,
  FeedbackCategory,
  QueueStatus,
  RaterKind,
} from "../router";
import type { AdminMessages } from "./messages";

const TOKEN_KEY = "judgebot-admin-token";

/** The token, for a tab whose storage is blocked. */
let held: string | null = null;

/** Take the token from the address's `#token=` (removing it there), else
 * the one this tab kept. */
export function takeToken(): string | null {
  const params = new URLSearchParams(window.location.hash.slice(1));
  const given = params.get("token");
  if (given !== null) {
    params.delete("token");
    const rest = params.toString();
    window.history.replaceState(
      window.history.state,
      "",
      `${window.location.pathname}${window.location.search}${rest === "" ? "" : `#${rest}`}`,
    );
    if (given !== "") {
      held = given;
      try {
        sessionStorage.setItem(TOKEN_KEY, given);
      } catch {
        // Storage blocked: the token lasts for this page only.
      }
    }
  }
  if (held !== null) return held;
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

/** Drop the token: the server refused it. */
export function forgetToken(): void {
  held = null;
  try {
    sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    // Nothing kept.
  }
}

/** A read: the value, or the status and the server's message (in `m` when
 * it sent none). Status 0 is no answer at all. */
export type Read<T> = { ok: true; value: T } | { ok: false; status: number; message: string };

async function read<T>(
  path: string,
  token: string,
  m: AdminMessages,
  valid: (v: unknown) => v is T,
): Promise<Read<T>> {
  let res: Response;
  try {
    res = await fetch(path, {
      headers: { authorization: `Bearer ${token}` },
      cache: "no-store",
    });
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
  return { ok: false, status: res.ok ? 502 : res.status, message };
}

function object(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null;
}

/** One stored call in the list (`judge_bot::admin::CallRow`). */
export interface CallRow {
  id: string;
  asked_at: string;
  interface: Interface;
  asker: string | null;
  asker_id: string | null;
  private: boolean;
  question: string;
  confidence: "low" | "medium" | "high";
  retired: string | null;
  models: string[];
  usd: number | null;
  cost_estimated: boolean;
}

/** A page of the list (`judge_bot::admin::CallsPage`). */
export interface CallsPage {
  calls: CallRow[];
  next: string | null;
  /** Every chat model a run has billed, for the model filter. */
  models: string[];
}

/** GET /admin/api/calls. */
export function fetchCalls(
  filter: AdminCallFilter,
  cursor: string | undefined,
  token: string,
  m: AdminMessages,
): Promise<Read<CallsPage>> {
  const q = new URLSearchParams();
  if (cursor) q.set("cursor", cursor);
  if (filter.interface) q.set("interface", filter.interface);
  if (filter.asker) q.set("asker", filter.asker);
  if (filter.model) q.set("model", filter.model);
  if (filter.private !== undefined) q.set("private", String(filter.private));
  if (filter.q) q.set("q", filter.q);
  const s = q.toString();
  return read(`/admin/api/calls${s === "" ? "" : `?${s}`}`, token, m, isCallsPage);
}

function isCallsPage(v: unknown): v is CallsPage {
  return (
    object(v) &&
    Array.isArray(v.calls) &&
    v.calls.every((c) => object(c) && typeof c.id === "string" && typeof c.question === "string") &&
    (v.next === null || typeof v.next === "string") &&
    Array.isArray(v.models)
  );
}

/** One billed request of a run (`judge_bot::admin::ChargeRow`). */
export interface Charge {
  stage: "extraction" | "synthesis" | "tool_round" | "embedding";
  provider: string;
  model: string;
  input_tokens: number | null;
  output_tokens: number | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  usd: number;
  estimated: boolean;
}

/** A run that left the call (`judge_bot::admin::RunRow`). */
export interface Run {
  id: string;
  started_at: string;
  elapsed_ms: number;
  outcome: string;
  retried: boolean;
  tool_round: boolean;
  charges: Charge[];
}

/** A rating's score. */
export type Score = "correct" | "partial" | "incorrect";

/** One rating with its rater and reason (`judge_bot::admin::RatingRow`). */
export interface Rating {
  user: string;
  kind: RaterKind;
  name: string | null;
  score: Score;
  judge: boolean;
  at: string;
  category: FeedbackCategory | null;
  wrong: string | null;
  right: string | null;
}

/** Everything stored about a call (`judge_api::admin::CallView`). */
export interface CallDetail {
  id: string;
  asked_at: string;
  interface: Interface;
  asker: string | null;
  asker_id: string | null;
  private: boolean;
  thread: string;
  locale: string;
  session: string | null;
  question: string;
  answer: string;
  confidence: "low" | "medium" | "high";
  source: "cr" | "commander";
  category: string;
  cr_version: string;
  citations: CitationView[];
  cards: { name: string; url: string }[];
  retired: string | null;
  ratings: Rating[];
  runs: Run[];
}

/** GET /admin/api/calls/{id}. */
export function fetchCall(id: string, token: string, m: AdminMessages): Promise<Read<CallDetail>> {
  return read(`/admin/api/calls/${encodeURIComponent(id)}`, token, m, isCallDetail);
}

function isCallDetail(v: unknown): v is CallDetail {
  return (
    object(v) &&
    typeof v.id === "string" &&
    typeof v.answer === "string" &&
    Array.isArray(v.citations) &&
    Array.isArray(v.ratings) &&
    Array.isArray(v.runs)
  );
}

/** A failed call (`judge_bot::db::stats::FailedRow`). */
export interface Failure {
  id: number;
  at: string;
  thread: string;
  interface: Interface;
  asker: string | null;
  private: boolean;
  question: string;
  error: string;
  first_rejection: string | null;
  attempts: string[];
}

/** GET /admin/api/failures. */
export function fetchFailures(
  token: string,
  m: AdminMessages,
): Promise<Read<{ failures: Failure[] }>> {
  return read("/admin/api/failures?limit=100", token, m, isFailures);
}

function isFailures(v: unknown): v is { failures: Failure[] } {
  return object(v) && Array.isArray(v.failures);
}

/** One day of usage (`judge_bot::db::stats::Day`). */
export interface Day {
  day: string;
  discord: number;
  web: number;
  mcp: number;
  cli: number;
  agent_session: number;
  agent: number;
  private: number;
  usd: number;
  llm_calls: number;
}

/** What a model was billed for (`judge_bot::db::stats::ModelUse`). */
export interface ModelUse {
  provider: string;
  model: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  usd: number;
  estimated: number;
}

/** A data refresh run (`judge_bot::ingest::runs::RecentRun`). */
export interface RefreshRun {
  started_at: string;
  trigger: string;
  process: string;
  outcome: "ok" | "failed" | "stopped" | "running" | "abandoned";
  cr_before: string | null;
  cr_after: string | null;
  failed_steps: string[];
}

/** Usage over a window (`judge_bot::db::stats::Usage`). */
export interface Usage {
  days: number;
  by_day: Day[];
  questions: number;
  usd: number;
  runs: number;
  run_usd: number;
  by_model: ModelUse[];
  outcomes: { outcome: string; runs: number }[];
  ratings: [number, number, number];
  retired_calls: number;
  lowest_rated: {
    call: string;
    question: string;
    effective_score: number;
    ratings: number;
    retired: boolean;
  }[];
  /** The feedback queue by category (`judge_bot::feedback::QueueCount`). */
  feedback: QueueCount[];
  refresh_runs: RefreshRun[];
  refresh_runs_note: string | null;
}

/** Items per category by status; a null category is an item re-rated
 * Correct. */
export interface QueueCount {
  category: FeedbackCategory | null;
  open: number;
  resolved: number;
  dismissed: number;
}

/** GET /admin/api/usage. */
export function fetchUsage(days: number, token: string, m: AdminMessages): Promise<Read<Usage>> {
  return read(`/admin/api/usage?days=${days}`, token, m, isUsage);
}

function isUsage(v: unknown): v is Usage {
  return (
    object(v) &&
    Array.isArray(v.by_day) &&
    Array.isArray(v.by_model) &&
    Array.isArray(v.outcomes) &&
    Array.isArray(v.ratings) &&
    Array.isArray(v.lowest_rated) &&
    Array.isArray(v.refresh_runs)
  );
}

/** One feedback item (`judge_bot::feedback::QueueItem`). */
export interface QueueItem {
  id: string;
  call: string;
  status: QueueStatus;
  opened_at: string;
  resolved_at: string | null;
  note: string | null;
  fixed_by: string | null;
  /** The rating as it stands now. */
  score: Score;
  category: FeedbackCategory | null;
  wrong: string | null;
  right: string | null;
  rater_kind: RaterKind;
  rater: string;
  rater_name: string | null;
  judge: boolean;
  ip: string | null;
  user_agent: string | null;
  rated_at: string;
  interface: Interface;
  private: boolean;
  question: string;
  retired: string | null;
}

/** A page of the queue (`judge_bot::feedback::QueuePage`). */
export interface QueuePage {
  items: QueueItem[];
  next: string | null;
}

/** GET /admin/api/queue. */
export function fetchQueue(
  filter: AdminQueueFilter,
  cursor: string | undefined,
  token: string,
  m: AdminMessages,
): Promise<Read<QueuePage>> {
  const q = new URLSearchParams();
  if (cursor) q.set("cursor", cursor);
  if (filter.status) q.set("status", filter.status);
  if (filter.category) q.set("category", filter.category);
  if (filter.interface) q.set("interface", filter.interface);
  if (filter.rater) q.set("rater", filter.rater);
  const s = q.toString();
  return read(`/admin/api/queue${s === "" ? "" : `?${s}`}`, token, m, isQueuePage);
}

function isQueueItem(v: unknown): v is QueueItem {
  return (
    object(v) &&
    typeof v.id === "string" &&
    typeof v.call === "string" &&
    typeof v.status === "string" &&
    typeof v.question === "string"
  );
}

function isQueuePage(v: unknown): v is QueuePage {
  return (
    object(v) &&
    Array.isArray(v.items) &&
    v.items.every(isQueueItem) &&
    (v.next === null || typeof v.next === "string")
  );
}

/** One item with its call (`judge_api::admin::ItemView`). */
export interface ItemView {
  item: QueueItem;
  call: CallDetail;
}

/** GET /admin/api/queue/{id}. */
export function fetchItem(id: string, token: string, m: AdminMessages): Promise<Read<ItemView>> {
  return read(`/admin/api/queue/${encodeURIComponent(id)}`, token, m, isItemView);
}

function isItemView(v: unknown): v is ItemView {
  return object(v) && isQueueItem(v.item) && isCallDetail(v.call);
}

/** POST /admin/api/queue/{id}/resolve or …/dismiss: the item as it now
 * stands. */
export async function closeItem(
  id: string,
  closing: "resolve" | "dismiss",
  body: { note: string; fixed_by?: string },
  token: string,
  m: AdminMessages,
): Promise<Read<QueueItem>> {
  let res: Response;
  try {
    res = await fetch(`/admin/api/queue/${encodeURIComponent(id)}/${closing}`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify(body),
      cache: "no-store",
    });
  } catch {
    return { ok: false, status: 0, message: m.unreachable };
  }
  const reply: unknown = await res.json().catch(() => null);
  if (res.ok && isQueueItem(reply)) return { ok: true, value: reply };
  const message =
    object(reply) && typeof reply.message === "string" ? reply.message : m.strangeReply(res.status);
  return { ok: false, status: res.ok ? 502 : res.status, message };
}
