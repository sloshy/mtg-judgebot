// The strings the site's own components show (HeaderLinks.astro, Footer.astro),
// one object per locale of src/locales.mjs. `satisfies` makes a locale added
// there without its strings here a type error (`astro check`). Starlight's own
// interface strings (search, "On this page", …) come with Starlight.
//
// A message with links is a list of parts, so each language puts the links
// where its word order wants them; the components own the URLs.

import type { locales } from "./locales.mjs";

/** A key of src/locales.mjs: `root` (English) or a URL directory. */
export type SiteLocale = keyof typeof locales;

/** A link a message places; the component knows where each one goes. */
export type LinkTo = "policy" | "scryfall" | "yawgatog" | "attribution";

/** Text, or a link with its label. */
export type Part = string | { link: LinkTo; label: string };

export type Messages = {
  /** The header link into the docs. Short: on a 320px phone it is the only
   * navigation in the header, beside the site title. */
  docs: string;
  /** The Fan Content Policy statement. */
  fanContent: readonly Part[];
  /** Where the data comes from, and the attribution page. */
  dataSources: readonly Part[];
};

const MESSAGES = {
  root: {
    docs: "Docs",
    fanContent: [
      "MTG Judgebot is unofficial Fan Content permitted under the ",
      { link: "policy", label: "Fan Content Policy" },
      ". Not approved or endorsed by Wizards of the Coast. Portions of the materials used are property of Wizards of the Coast. ©Wizards of the Coast LLC.",
    ],
    dataSources: [
      "The Comprehensive Rules come from Wizards of the Coast. Card data, rulings and card symbols come from ",
      { link: "scryfall", label: "Scryfall" },
      ", which is not affiliated with this project. Rule links go to the ",
      { link: "yawgatog", label: "Yawgatog" },
      " mirror. ",
      { link: "attribution", label: "License and attribution" },
      ".",
    ],
  },
} as const satisfies Record<SiteLocale, Messages>;

function isSiteLocale(key: string): key is SiteLocale {
  return Object.hasOwn(MESSAGES, key);
}

/** The key of a page's locale: Starlight's route `locale`, `undefined` at the root. */
export function siteLocale(routeLocale: string | undefined): SiteLocale {
  const key = routeLocale ?? "root";
  // Starlight's locales are src/locales.mjs, so every route's key is one.
  return isSiteLocale(key) ? key : "root";
}

/** The strings for a page in `locale`. */
export function messages(locale: SiteLocale): Messages {
  return MESSAGES[locale];
}

/** `path` (from the docs root, starting with `/`) in `locale`, under the site's
 * base: these components render at every depth, so their links cannot be
 * relative the way the Markdown's are. */
export function localePath(locale: SiteLocale, path: string): string {
  const base = import.meta.env.BASE_URL.replace(/\/$/, "");
  return locale === "root" ? `${base}${path}` : `${base}/${locale}${path}`;
}
