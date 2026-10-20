// Every string the admin pages show, one catalog per locale, as the public
// page's (../messages.ts): a new locale is one more object `satisfies
// AdminMessages`, and TypeScript names every message it is missing. What the
// server writes (an error's `message`, a citation's `label`) comes as sent.

import type { Interface } from "../api";
import type { Locale } from "../messages";

export type AdminMessages = {
  /** The bar's name for the admin pages. */
  title: string;
  /** The tabs, and their accessible name. */
  navLabel: string;
  navCalls: string;
  navFailures: string;
  navUsage: string;

  /** No token in the address or this tab. */
  noTokenTitle: string;
  noToken: string;
  /** The server refused the token. */
  badTokenTitle: string;
  badToken: string;
  /** The command that prints the address with the token. */
  adminUrlCommand: string;

  /** Replies the page makes itself when the server's is missing. */
  unreachable: string;
  strangeReply: (status: number) => string;
  errorTitle: string;
  maintenanceTitle: string;
  missingTitle: string;
  pageMissing: string;

  callsTitle: string;
  callsIntro: string;
  /** The filters: their labels, the "any" choice, the private choice. */
  filterSearch: string;
  filterSearchPlaceholder: string;
  filterInterface: string;
  filterAsker: string;
  filterAskerPlaceholder: string;
  filterModel: string;
  filterVisibility: string;
  any: string;
  onlyPrivate: string;
  onlyPublic: string;
  applyFilters: string;
  clearFilters: string;
  callsEmpty: string;
  loadMore: string;
  loading: string;

  /** Each interface's name. */
  interfaceName: Record<Interface, string>;
  /** The tag on a private call. */
  privateTag: string;
  publicTag: string;
  retiredTag: string;
  anonymous: string;
  /** A Discord asker known by id only (forgotten names are anonymous). */
  askerId: (id: string) => string;
  /** A cost in US dollars, and one that includes a worst-case estimate. */
  usd: (usd: number, estimated: boolean) => string;
  /** No run, so no cost (an agent session). */
  noRun: string;
  /** When something happened, from an RFC 3339 time. */
  when: (iso: string) => string;
  /** A day (`YYYY-MM-DD`). */
  day: (iso: string) => string;

  backToCalls: string;
  callMissingTitle: string;
  /** The heading over a call's stored facts, and each fact's name. */
  factsTitle: string;
  factAsked: string;
  factInterface: string;
  factAsker: string;
  factVisibility: string;
  factThread: string;
  factLocale: string;
  factSession: string;
  factCategory: string;
  factRules: string;
  factRetired: string;
  ratingsTitle: (count: number) => string;
  ratingsEmpty: string;
  /** One rating: the rater's id, the score and whether a judge gave it. */
  ratingLine: (user: string, score: Rating, judge: boolean) => string;
  runsTitle: string;
  runsEmpty: string;
  /** A run's lead line: its outcome, length and the retry / tool round. */
  runLine: (outcome: string, ms: number, retried: boolean, toolRound: boolean) => string;
  /** A billed request's line: stage, provider/model, tokens. */
  stageName: Record<"extraction" | "synthesis" | "tool_round" | "embedding", string>;
  tokens: (input: number | null, output: number | null) => string;

  failuresTitle: string;
  failuresIntro: string;
  failuresEmpty: string;
  failureError: string;
  failureRejection: string;
  failureAttempt: (n: number) => string;

  usageTitle: string;
  /** The window's choices, and their accessible name. */
  windowLabel: string;
  lastDays: (days: number) => string;
  statQuestions: string;
  statSpend: string;
  statRuns: string;
  statRunSpend: string;
  /** The spend chart: its title and a bar's hover text. */
  chartTitle: string;
  chartBar: (day: string, usd: string) => string;
  /** Under the chart: what its tallest bar stands for. */
  chartMax: (usd: string) => string;
  byDayTitle: string;
  dayLine: (questions: number, privateCount: number, usd: string, calls: number) => string;
  byModelTitle: string;
  modelLine: (requests: number, tokens: string, estimated: number) => string;
  outcomesTitle: string;
  ratingsSummary: (r: [number, number, number]) => string;
  retiredCalls: (n: number) => string;
  lowestTitle: string;
  lowestLine: (score: number, ratings: number) => string;
  refreshTitle: string;
  refreshLine: (r: { trigger: string; process: string; outcome: string }) => string;
  refreshFailedSteps: (steps: readonly string[]) => string;
  nothingYet: string;
};

/** A rating's score. */
type Rating = "correct" | "partial" | "incorrect";

const dateTime = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short" });
const dateOnly = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeZone: "UTC" });
const count = new Intl.NumberFormat("en");

export const en = {
  title: "Admin",
  navLabel: "Admin pages",
  navCalls: "Calls",
  navFailures: "Failures",
  navUsage: "Usage",

  noTokenTitle: "Token needed",
  noToken:
    "These pages need the admin token. Open the address this command prints, on the machine that runs judgebot:",
  badTokenTitle: "Token refused",
  badToken:
    "The server did not accept this token: it may have changed. Open the address this command prints:",
  adminUrlCommand: "judge-cli admin-url",

  unreachable: "The server is unreachable. Is judgebot running with --admin?",
  strangeReply: (status) => `The server answered strangely (HTTP ${status}). Please try again.`,
  errorTitle: "Error",
  maintenanceTitle: "Maintenance",
  missingTitle: "Page not found",
  pageMissing: "There is nothing at this address.",

  callsTitle: "Calls",
  callsIntro: "Every stored call, private ones included, newest first.",
  filterSearch: "Text",
  filterSearchPlaceholder: "Question, answer or username",
  filterInterface: "Interface",
  filterAsker: "Asker",
  filterAskerPlaceholder: "Username or Discord id",
  filterModel: "Model",
  filterVisibility: "Visibility",
  any: "Any",
  onlyPrivate: "Private only",
  onlyPublic: "Public only",
  applyFilters: "Filter",
  clearFilters: "Clear",
  callsEmpty: "No calls match.",
  loadMore: "Show older calls",
  loading: "Loading…",

  interfaceName: {
    discord: "Discord",
    web: "Web",
    mcp: "MCP",
    cli: "judge-cli",
    agent_session: "Agent session",
    agent: "Agent",
  },
  privateTag: "Private",
  publicTag: "Public",
  retiredTag: "Retired",
  anonymous: "Anonymous",
  askerId: (id) => `Discord user ${id}`,
  usd: (usd, estimated) => `$${usd.toFixed(4)}${estimated ? " (at most)" : ""}`,
  noRun: "no run",
  when: (iso) => {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : dateTime.format(d);
  },
  day: (iso) => {
    const d = new Date(`${iso}T00:00:00Z`);
    return Number.isNaN(d.getTime()) ? iso : dateOnly.format(d);
  },

  backToCalls: "Calls",
  callMissingTitle: "Call not found",
  factsTitle: "Stored with it",
  factAsked: "Asked",
  factInterface: "Interface",
  factAsker: "Asker",
  factVisibility: "Visibility",
  factThread: "Thread",
  factLocale: "Language",
  factSession: "Agent session",
  factCategory: "Category",
  factRules: "Comprehensive Rules",
  factRetired: "Retired",
  ratingsTitle: (n) => (n === 1 ? "1 rating" : `${n} ratings`),
  ratingsEmpty: "Not rated.",
  ratingLine: (user, score, judge) =>
    `${user}: ${{ correct: "correct", partial: "partially correct", incorrect: "incorrect" }[score]}${judge ? " (judge)" : ""}`,
  runsTitle: "Run",
  runsEmpty:
    "No run recorded: an agent session (its model is the agent), or a call from before runs were recorded.",
  runLine: (outcome, ms, retried, toolRound) =>
    [
      outcome.replaceAll("_", " "),
      `${(ms / 1000).toFixed(1)} s`,
      ...(retried ? ["retried"] : []),
      ...(toolRound ? ["looked rules up"] : []),
    ].join(" · "),
  stageName: {
    extraction: "Extraction",
    synthesis: "Synthesis",
    tool_round: "Tool round",
    embedding: "Embedding",
  },
  tokens: (input, output) =>
    input === null || output === null
      ? "no usage reported"
      : `${count.format(input)} in · ${count.format(output)} out`,

  failuresTitle: "Failures",
  failuresIntro:
    "Calls that failed for a reason to look into, kept 30 days: the question, the error and what the model sent.",
  failuresEmpty: "No failed calls.",
  failureError: "Error",
  failureRejection: "First attempt rejected",
  failureAttempt: (n) => `Attempt ${n}`,

  usageTitle: "Usage",
  windowLabel: "Window",
  lastDays: (days) => (days === 366 ? "A year" : `${days} days`),
  statQuestions: "Questions",
  statSpend: "Spend",
  statRuns: "Runs",
  statRunSpend: "Spend on runs",
  chartTitle: "Spend per day (USD)",
  chartBar: (day, usd) => `${day}: ${usd}`,
  chartMax: (usd) => `highest: ${usd}`,
  byDayTitle: "By day",
  dayLine: (questions, privateCount, usd, calls) =>
    `${questions} ${questions === 1 ? "question" : "questions"}${privateCount > 0 ? ` (${privateCount} private)` : ""} · ${usd} · ${calls} model ${calls === 1 ? "call" : "calls"}`,
  byModelTitle: "Runs by model",
  modelLine: (requests, tokens, estimated) =>
    `${requests} ${requests === 1 ? "request" : "requests"} · ${tokens}${estimated > 0 ? ` · ${estimated} at the worst case` : ""}`,
  outcomesTitle: "Runs by outcome",
  ratingsSummary: ([incorrect, partial, correct]) =>
    `Ratings: ${correct} correct, ${partial} partially correct, ${incorrect} incorrect.`,
  retiredCalls: (n) => `${n} ${n === 1 ? "call is" : "calls are"} retired.`,
  lowestTitle: "Lowest rated",
  lowestLine: (score, ratings) =>
    `score ${score.toFixed(2)} · ${ratings} ${ratings === 1 ? "rating" : "ratings"}`,
  refreshTitle: "Data refreshes",
  refreshLine: (r) => `${r.outcome} · ${r.trigger} · ${r.process}`,
  refreshFailedSteps: (steps) => `Failed: ${steps.join(", ")}`,
  nothingYet: "Nothing yet.",
} satisfies AdminMessages;

const CATALOGS: Record<Locale, AdminMessages> = { en };

export function adminMessages(locale: Locale): AdminMessages {
  return CATALOGS[locale];
}
