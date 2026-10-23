// Every string the page shows a visitor, one catalog per locale. A new locale
// is one more object `satisfies Messages` added to CATALOGS and LOCALES, and
// TypeScript names every message it is missing.
//
// Templates with values are functions, so each locale owns its word order,
// plurals and punctuation. A message that wraps a link or a code span takes
// the element (or a function from the link's label to it) as an argument and
// returns the pieces in its own order; the components build the elements, so
// the catalog never holds a URL or markup.
//
// What the server writes (a reply's `message`, a citation's `label`) is not
// here: the page sends its locale with each question and shows the server's
// text as it comes.

import type { JSX } from "solid-js";
import type { FeedEntry, Interface, Outcomes, RatingCounts, Score } from "./api";
import type { FeedbackCategory } from "./router";

/** The locales the page has a catalog for. */
export const LOCALES = ["en"] as const;
export type Locale = (typeof LOCALES)[number];

/** A link whose label the message chooses. */
export type Link = (label: string) => JSX.Element;

export type Messages = {
  /** The page heading. */
  title: string;
  tagline: string;
  /** Shown while an answer is on its way. */
  thinking: string;
  examplesTitle: string;
  /** Example questions offered before the first one is asked. */
  examples: readonly string[];
  placeholder: string;
  ask: string;
  judging: string;

  /** A citation's verbatim quote, in the locale's quotation marks. */
  quoted: (quote: JSX.Element) => JSX.Element;
  cardsLabel: string;
  /** Leads the unreleased-card note. */
  noteLabel: string;
  /** Above an answer naming cards that are not out yet and that the loaded
   * Comprehensive Rules may not cover: their names, joined. */
  unreleasedNote: (names: string) => string;
  /** The same, on a stored answer: what it said when it was given. */
  unreleasedNoteStored: (names: string) => string;
  /** Between two items of an inline list (the cards, the footer's data line). */
  separator: string;
  confidence: Record<"low" | "medium" | "high", string>;
  source: Record<"cr" | "commander", string>;
  /** The line under an answer. */
  answerMeta: (confidence: string, crDate: string, source: string) => string;
  /** A Comprehensive Rules version (`YYYYMMDD`) as a date. */
  crDate: (version: string) => string;

  /** "Did you mean…?" for the span `query`. */
  whichCard: (query: JSX.Element) => JSX.Element;
  choicesTruncated: string;
  /** The spans still to be asked about after this one. */
  askAfter: (queries: readonly string[]) => string;
  notFound: (names: readonly string[]) => string;
  tryAgain: string;

  /** Replies the page makes itself when the server's is missing. */
  unreachable: string;
  strangeReply: (status: number) => string;

  notRated: string;
  fanContent: (policy: Link) => JSX.Element;
  dataSources: (scryfall: Link, yawgatog: Link) => JSX.Element;

  /** The footer's data line, from its parts. */
  freshnessLine: (parts: readonly string[]) => string;
  freshnessUnknown: string;
  noRules: string;
  rulesVersion: (date: string) => string;
  refreshed: (secsAgo: number) => string;
  noSuccessfulRefresh: string;
  noRefreshRecorded: string;
  refreshFailed: string;
  /** An age as words: `under a minute ago`, `5 minutes ago`. */
  ago: (secs: number) => string;

  /** The source offer while GET /api/about has not answered. */
  offerUnanswered: (license: Link, endpoint: JSX.Element) => JSX.Element;
  /** The source offer from the server's facts. `commit` is
   * [`commitUnknown`] or [`commitKnown`]. */
  offer: (facts: {
    program: string;
    copyright: string;
    license: JSX.Element;
    source: Link;
    commit: JSX.Element;
  }) => JSX.Element;
  commitUnknown: string;
  commitKnown: (hash: JSX.Element, dirty: boolean) => JSX.Element;
  /** The operator's contact, after the source offer. */
  support: (email: JSX.Element, discord: JSX.Element | null) => JSX.Element;

  /** The links between the page's routes. */
  navAsk: string;
  navFeed: string;
  /** Under the question box when the feed is served. */
  askIsPublic: string;
  feedTitle: string;
  feedIntro: string;
  /** The feed's filter for every interface. */
  feedAll: string;
  /** Each interface's name, as the feed labels an entry. */
  interfaceName: Record<Interface, string>;
  /** An entry with no username. */
  anonymous: string;
  /** An entry's lead line: who, where, when (a link to its own page). */
  entryMeta: (asker: JSX.Element, via: string, when: JSX.Element) => JSX.Element;
  /** When an entry was asked, from its RFC 3339 time. */
  askedAt: (iso: string) => string;
  /** How an entry was rated: the counts shown and the judge's rating. */
  ratingsLine: (r: RatingCounts, judge: FeedEntry["ratings"]["judge"]) => string;
  /** The button that adds the web's ratings to the counts, given how many. */
  includeWebRatings: (n: number) => string;
  /** The button that takes them away again. */
  hideWebRatings: string;
  /** Over raters' reasons, when the operator shows them. */
  reasonsTitle: string;

  /** Rating an answer: the buttons' lead, each score and kind of problem. */
  rateAsk: string;
  scoreName: Record<Score, string>;
  categoryName: Record<FeedbackCategory, string>;
  /** The reason form: its title, its fields, its note and buttons. */
  rateFormTitle: (fault: "partial" | "incorrect") => string;
  rateCategory: string;
  rateChoose: string;
  rateWrong: string;
  rateRight: string;
  rateName: string;
  rateOptional: string;
  rateFormNote: string;
  /** The same, when the operator shows reasons publicly. */
  rateFormNotePublic: string;
  rateSend: string;
  rateCancel: string;
  rateSending: string;
  /** "You already rated this…": replace it, or keep it. */
  rateReplace: string;
  rateKeep: string;
  /** After Keep: the rating kept, and its category when it had one. */
  rateKept: (score: string, category: string | null) => string;
  /** Under an entry whose citations no longer hold. */
  retiredNote: (reason: string) => string;
  loading: string;
  loadMore: string;
  feedEmpty: string;
  /** `/users/<name>`'s heading. */
  userTitle: (name: string) => string;
  /** A route that names nothing. */
  pageMissing: string;
  /** A link back to the question box. */
  backToAsk: string;
  /** The feed's search box: its label, the hint inside it, its button. */
  searchLabel: string;
  searchPlaceholder: string;
  searchButton: string;
  /** A search that matched nothing, given what was searched for. */
  searchEmpty: (text: string) => string;

  /** Before the visitor's own question, for screen readers. */
  you: string;
  /** Under a question, a "did you mean…?" choice it was re-asked with. */
  pinned: (span: string, name: string) => string;
  /** The heading over an answer's citations. */
  citationsTitle: (count: number) => string;
  /** The badge on a citation of the Comprehensive Rules. */
  ruleBadge: string;
  /** Under the question box on a keyboard. */
  keyHint: string;
  /** The conversation on the ask page, for screen readers. */
  conversationLabel: string;
  /** The links between the page's routes, for screen readers. */
  navLabel: string;
  /** The feed's interface filters, for screen readers. */
  filtersLabel: string;
  /** A divider between days in the feed, from an RFC 3339 time. */
  dayLabel: (iso: string) => string;
  /** The time of day an entry was asked, from its RFC 3339 time. */
  askedTime: (iso: string) => string;
  /** Unfolds a long answer in the feed, and folds it again. */
  showMore: string;
  showLess: string;

  /** The header's link to the documentation site. */
  navDocs: string;
  /** A link past the header to the page's content. */
  skipToContent: string;
  /** The theme picker: its accessible name and its choices. */
  themeLabel: string;
  themeName: Record<"auto" | "dark" | "light", string>;
  /** The title of the notice each reply other than an answer or a
   * "did you mean…?" comes in. */
  replyTitle: Record<"not_found" | "error" | "busy" | "rate_limited" | "maintenance", string>;
  /** The title of the notice on an address that names nothing. */
  missingTitle: string;
  /** The title of the notice on `/calls/<id>` when there is no such call. */
  callMissingTitle: string;
  /** The feed's two layouts. */
  viewLabel: string;
  viewCompact: string;
  viewExpanded: string;
  /** The tag on a retired entry in the compact feed. */
  retiredBadge: string;
  /** An answer's confidence, as the compact feed tags it. */
  confidenceTag: (confidence: string) => string;

  // ---- the dashboard (`/dashboard`, and the admin page's head) ----
  navDashboard: string;
  dashboardTitle: string;
  dashboardIntro: string;
  /** The heading when one Discord user's questions are shown. */
  dashboardUserTitle: (name: string) => string;
  /** The range's choices, and their accessible name. */
  rangeLabel: string;
  lastDays: (days: number) => string;
  /** The model filter's accessible name and its "every model" choice. */
  modelFilterLabel: string;
  allModels: string;
  /** Links from one user's figures: their answers, and everyone's figures. */
  userAnswers: (name: string) => string;
  everyone: string;
  /** From a user's page: their figures on the dashboard. */
  userStatsLink: string;
  /** The range the figures cover, from two `YYYY-MM-DD` days. */
  rangeLine: (from: string, to: string) => string;
  /** The public window, when the operator hides older questions. */
  windowNote: (days: number) => string;
  /** A whole number, and a share as a percentage. */
  count: (n: number) => string;
  percent: (part: number, whole: number) => string;
  /** A cost in US dollars, and whether it includes a worst-case estimate. */
  usd: (usd: number, estimated: boolean) => string;
  /** The headline figures. */
  statQuestions: string;
  statAskers: string;
  statAnswered: string;
  /** The questions-over-time chart: its title, accessible summary, the
   * table that is its text alternative, and a bar's hover text. */
  chartTitle: string;
  chartSummary: (from: string, to: string, total: number, peak: number) => string;
  chartTable: string;
  chartDay: string;
  chartWeekOf: (day: string) => string;
  chartTotal: string;
  chartPeak: (n: number) => string;
  /** A day (`YYYY-MM-DD`) as a short date. */
  shortDay: (day: string) => string;
  /** The interfaces' totals, each a link into the feed. */
  interfacesTitle: string;
  outcomesTitle: string;
  outcomeName: Record<keyof Outcomes, string>;
  /** Under a user filter: runs do not record who asked. */
  outcomesPerUser: string;
  ratingsTitle: string;
  /** The rating mix's web toggle. */
  includeWebToggle: (n: number) => string;
  /** No ratings in the range. */
  noRatings: string;
  categoriesTitle: string;
  cardsTitle: string;
  askersTitle: string;
  costTitle: string;
  costTotal: string;
  costPerQuestion: string;
  modelsTitle: string;
  /** Beside a model's answers: how many questions it read (extraction). */
  modelRead: (extracted: number) => string;
  /** No questions in the range, and nothing in a list. */
  dashboardEmpty: string;
  nothingYet: string;
};

/** `20260819` → `2026-08-19` (matching the Discord footer). */
function isoDate(v: string): string {
  return /^\d{8}$/.test(v) ? `${v.slice(0, 4)}-${v.slice(4, 6)}-${v.slice(6, 8)}` : v;
}

/** An age as words, as the server's English catalog (`judge_core::locale`) writes it. */
function enAgo(secs: number): string {
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

const enCount = new Intl.NumberFormat("en");

/** `YYYY-MM-DD` (a UTC day) as `Sep 4` or, with the year, `Sep 4, 2026`. */
function enShortDay(day: string, year: boolean): string {
  const d = new Date(`${day}T00:00:00Z`);
  return Number.isNaN(d.getTime())
    ? day
    : new Intl.DateTimeFormat("en", {
        month: "short",
        day: "numeric",
        year: year ? "numeric" : undefined,
        timeZone: "UTC",
      }).format(d);
}

export const en = {
  title: "MTG Judgebot",
  tagline: "Ask a Magic: The Gathering rules question; the answer cites the Comprehensive Rules.",
  thinking: "Consulting the rules… this usually takes 20–45 seconds.",
  examplesTitle: "Try one",
  // Each is a common ruling with a short, unambiguous answer, and between
  // them they exercise a nickname ("bob", "goyf"), bracketed names and the
  // Commander rules.
  examples: [
    "I attack with a 5/5 that has trample and deathtouch and they block with a 4/4. How much damage can I trample over?",
    "My opponent has bob and flips a goyf off the top. How much life does he lose?",
    "I control [[Blood Artist]] and my opponent casts [[Wrath of God]]. Does Blood Artist trigger for the other creatures that die with it?",
    "My commander is [[Kenrith, the Returned King]]. Can I run Nicol Bolas, the Ravager in the deck?",
  ],
  placeholder: "Does lifelink stack? Use brackets like [[Full Card Name]] to avoid ambiguity.",
  ask: "Ask the judge",
  judging: "Judging…",

  quoted: (quote) => ["“", quote, "”"],
  cardsLabel: "Cards:",
  noteLabel: "Note:",
  unreleasedNote: (names) =>
    `This question references unreleased cards. The Comprehensive Rules might not have enough information to properly evaluate one or more of them (${names}).`,
  unreleasedNoteStored: (names) =>
    `When this was answered, it referenced unreleased cards. The Comprehensive Rules then loaded might not have had enough information to properly evaluate one or more of them (${names}).`,
  separator: " · ",
  confidence: { low: "Low", medium: "Medium", high: "High" },
  source: { cr: "Comprehensive Rules", commander: "Commander rules" },
  answerMeta: (confidence, crDate, source) =>
    `Confidence: ${confidence} · CR ${crDate} · ${source}`,
  crDate: isoDate,

  whichCard: (query) => ["I'm not sure which card you mean by ", query, ". Did you mean…?"],
  choicesTruncated:
    "Showing the first five. If yours isn't here, re-ask writing its full name as [[Full Card Name]].",
  askAfter: (queries) => `After that I'll ask about ${queries.join(", ")}.`,
  notFound: (names) =>
    `I couldn't find a card called ${names.join(" or ")}. Check the spelling, or write the full name as [[Full Card Name]].`,
  tryAgain: "Try again",

  unreachable: "The judge is unreachable. Is the server running?",
  strangeReply: (status) => `The judge answered strangely (HTTP ${status}). Please try again.`,

  notRated:
    "Ratings here are anonymous and never shape later answers. Answers are AI-generated; verify anything important with a human judge.",
  // Keep in step with site/src/components/Footer.astro.
  fanContent: (policy) => [
    "MTG Judgebot is unofficial Fan Content permitted under the ",
    policy("Fan Content Policy"),
    ". Not approved or endorsed by Wizards of the Coast. Portions of the materials used are property of Wizards of the Coast. ©Wizards of the Coast LLC.",
  ],
  dataSources: (scryfall, yawgatog) => [
    "The Comprehensive Rules come from Wizards of the Coast. Card data, rulings and card symbols come from ",
    scryfall("Scryfall"),
    ", which is not affiliated with this project. Rule links go to the ",
    yawgatog("Yawgatog"),
    " mirror.",
  ],

  freshnessLine: (parts) => `${parts.join(" · ")}.`,
  freshnessUnknown: "Data freshness unknown",
  noRules: "No Comprehensive Rules loaded",
  rulesVersion: (date) => `Comprehensive Rules ${date}`,
  refreshed: (secsAgo) => `refreshed ${enAgo(secsAgo)}`,
  noSuccessfulRefresh: "no successful refresh yet",
  noRefreshRecorded: "no refresh run recorded yet",
  refreshFailed: "latest refresh failed",
  ago: enAgo,

  offerUnanswered: (license, endpoint) => [
    "Free software under the ",
    license("GNU Affero General Public License"),
    ". The source offer for this instance (repository and commit) comes from ",
    endpoint,
    ", which has not answered.",
  ],
  offer: ({ program, copyright, license, source, commit }) => [
    `${program} — ${copyright}. Free software under the `,
    license,
    "; anyone offering a modified version over a network must offer its source under the same licence. ",
    source("Source code of this instance"),
    " ",
    commit,
    ".",
  ],
  commitUnknown: "(commit unknown)",
  commitKnown: (hash, dirty) => [
    "(commit ",
    hash,
    dirty ? ", built with uncommitted changes" : "",
    ")",
  ],
  support: (email, discord) => [
    " For support, contact whoever runs this instance: ",
    email,
    discord === null ? "" : [", or ", discord, " on Discord"],
    ".",
  ],

  navAsk: "Ask",
  navFeed: "Feed",
  askIsPublic:
    "Questions asked here are listed with their answers on the public feed, without a name.",
  feedTitle: "Recent answers",
  feedIntro:
    "Every question not asked privately, newest first: from Discord, from this page and from the agent tools.",
  feedAll: "All",
  interfaceName: {
    discord: "Discord",
    web: "Web",
    mcp: "MCP",
    cli: "judge-cli",
    agent_session: "Agent session",
    agent: "Agent",
  },
  anonymous: "Anonymous",
  entryMeta: (asker, via, when) => [asker, ` · ${via} · `, when],
  askedAt: (iso) => {
    const d = new Date(iso);
    return Number.isNaN(d.getTime())
      ? iso
      : new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short" }).format(d);
  },
  ratingsLine: (r, judgeScore) => {
    const total = r.correct + r.partial + r.incorrect;
    const judge =
      judgeScore === null
        ? ""
        : ` · a judge rated it ${{ correct: "correct", partial: "partially correct", incorrect: "incorrect" }[judgeScore]}`;
    return total === 0
      ? `Not rated yet${judge}.`
      : `Rated ${r.correct} correct, ${r.partial} partially correct, ${r.incorrect} incorrect${judge}.`;
  },
  includeWebRatings: (n) => `Include ${n} from the web`,
  hideWebRatings: "Hide web ratings",
  reasonsTitle: "Reasons given",

  rateAsk: "Was this right?",
  scoreName: { correct: "Correct", partial: "Partially correct", incorrect: "Incorrect" },
  categoryName: {
    wrong_conclusion: "Wrong conclusion",
    missed_part: "Missed part of the question",
    bad_citation: "A citation doesn't support it",
    wrong_card: "Wrong card",
    misread_question: "Misread the question",
    wrong_refusal: "Shouldn't have refused",
    outdated: "Outdated",
    other: "Other",
  },
  rateFormTitle: (fault) =>
    fault === "incorrect" ? "Incorrect: what went wrong?" : "Partially correct: what's missing?",
  rateCategory: "What kind of problem?",
  rateChoose: "Choose one",
  rateWrong: "What's wrong",
  rateRight: "The right answer",
  rateName: "Your name",
  rateOptional: "(optional)",
  rateFormNote:
    "Your reason goes to whoever runs this site, beside the question and answer. The judge never reads it.",
  rateFormNotePublic:
    "Your reason goes to whoever runs this site, and is shown under this answer on the public pages, without your name. The judge never reads it.",
  rateSend: "Send",
  rateCancel: "Cancel",
  rateSending: "Sending…",
  rateReplace: "Replace",
  rateKeep: "Keep",
  rateKept: (score, category) =>
    `Kept your earlier rating: ${score}${category === null ? "" : ` (${category})`}.`,
  retiredNote: (reason) => `No longer used as an example: ${reason}`,
  loading: "Loading…",
  loadMore: "Show older answers",
  feedEmpty: "Nothing here yet.",
  userTitle: (name) => `Questions asked by @${name}`,
  pageMissing: "There is nothing at this address.",
  backToAsk: "Ask a question",
  searchLabel: "Search the answers",
  searchPlaceholder: "Search, or @username",
  searchButton: "Search",
  searchEmpty: (text) => `No answers match “${text}”.`,

  you: "You: ",
  pinned: (span, name) => `“${span}” means ${name}`,
  citationsTitle: (count) => (count === 1 ? "1 citation" : `${count} citations`),
  ruleBadge: "CR",
  keyHint: "Enter sends · Shift+Enter adds a line",
  conversationLabel: "Conversation with the judge",
  navLabel: "Pages",
  filtersLabel: "Filter by interface",
  dayLabel: (iso) => {
    const d = new Date(iso);
    return Number.isNaN(d.getTime())
      ? iso
      : new Intl.DateTimeFormat("en", { dateStyle: "full" }).format(d);
  },
  askedTime: (iso) => {
    const d = new Date(iso);
    return Number.isNaN(d.getTime())
      ? iso
      : new Intl.DateTimeFormat("en", { timeStyle: "short" }).format(d);
  },
  showMore: "Show the whole answer",
  showLess: "Show less",

  navDocs: "Docs",
  skipToContent: "Skip to content",
  themeLabel: "Theme",
  themeName: { auto: "Auto", dark: "Dark", light: "Light" },
  replyTitle: {
    not_found: "Card not found",
    error: "Error",
    busy: "Busy",
    rate_limited: "Rate limited",
    maintenance: "Maintenance",
  },
  missingTitle: "Page not found",
  callMissingTitle: "Answer not found",
  viewLabel: "Layout",
  viewCompact: "Compact",
  viewExpanded: "Expanded",
  retiredBadge: "Retired",
  confidenceTag: (confidence) => `${confidence} confidence`,

  navDashboard: "Dashboard",
  dashboardTitle: "Dashboard",
  dashboardIntro:
    "What the judge has been asked and how it went: every question not asked privately, by day, interface, outcome, rating, category and card.",
  dashboardUserTitle: (name) => `Questions asked by @${name}`,
  rangeLabel: "Range",
  lastDays: (days) => (days >= 365 ? "A year" : `${days} days`),
  modelFilterLabel: "Model",
  allModels: "Every model",
  userAnswers: (name) => `@${name}'s answers`,
  everyone: "Everyone",
  userStatsLink: "Dashboard",
  rangeLine: (from, to) => {
    const f = enShortDay(from, true);
    const t = enShortDay(to, true);
    return from === to ? f : `${f} to ${t}`;
  },
  windowNote: (days) =>
    `Questions older than ${days} ${days === 1 ? "day" : "days"} are not shown or counted here.`,
  count: (n) => enCount.format(n),
  percent: (part, whole) => (whole === 0 ? "–" : `${Math.round((100 * part) / whole)}%`),
  usd: (usd, estimated) =>
    `$${usd < 1 ? usd.toFixed(4) : usd.toFixed(2)}${estimated ? " (at most)" : ""}`,
  statQuestions: "Questions",
  statAskers: "Discord askers",
  statAnswered: "Answered",
  chartTitle: "Questions per day",
  chartSummary: (from, to, total, peak) =>
    `Questions per day by interface, ${from} to ${to}: ${enCount.format(total)} in all, at most ${enCount.format(peak)} in one bar.`,
  chartTable: "Show as a table",
  chartDay: "Day",
  chartWeekOf: (day) => `Week of ${enShortDay(day, false)}`,
  chartTotal: "Total",
  chartPeak: (n) => `highest: ${enCount.format(n)}`,
  shortDay: (day) => enShortDay(day, false),
  interfacesTitle: "By interface",
  outcomesTitle: "How it went",
  outcomeName: {
    answered: "Answered",
    ambiguous: "Asked which card",
    not_found: "Card not found",
    out_of_scope: "Out of scope",
    refused: "Declined",
    failed: "Failed",
  },
  outcomesPerUser:
    "Outcomes are not counted per user: a question that was not answered records no asker.",
  ratingsTitle: "Ratings",
  includeWebToggle: (n) => `Include ${enCount.format(n)} from the web`,
  noRatings: "No ratings in this range.",
  categoriesTitle: "Most-asked topics",
  cardsTitle: "Most-asked cards",
  askersTitle: "Most active askers",
  costTitle: "Cost",
  costTotal: "Model cost of these answers",
  costPerQuestion: "Per question",
  modelsTitle: "Models",
  modelRead: (extracted) => `read ${enCount.format(extracted)}`,
  dashboardEmpty: "No questions in this range.",
  nothingYet: "Nothing yet.",
} satisfies Messages;

const CATALOGS: Record<Locale, Messages> = { en };

export function messages(locale: Locale): Messages {
  return CATALOGS[locale];
}

export function isLocale(v: unknown): v is Locale {
  return LOCALES.some((l) => l === v);
}

/** The first of `preferred` (BCP 47 tags such as `en-GB`, as
 * `navigator.languages` lists them) whose language subtag is a locale we
 * have, else `fallback`. */
export function pickLocale<F>(preferred: readonly string[], fallback: F): Locale | F {
  for (const tag of preferred) {
    const language = tag.split("-", 1)[0]?.toLowerCase();
    const hit = LOCALES.find((l) => l === language);
    if (hit !== undefined) return hit;
  }
  return fallback;
}
