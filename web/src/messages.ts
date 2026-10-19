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
    "Answers here can't be rated. Answers are AI-generated; verify anything important with a human judge.",
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
