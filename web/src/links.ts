// Addresses outside this page. The documentation is the project's site (its
// default SITE_URL, site/astro.config.mjs); a build for a fork that publishes
// its own sets VITE_DOCS_URL.

const configured: unknown = import.meta.env.VITE_DOCS_URL;

/** The documentation site's root, without a trailing slash. */
export const DOCS_URL = (
  typeof configured === "string" && configured !== ""
    ? configured
    : "https://mtg-judgebot.rpeters.dev"
).replace(/\/$/, "");

/** A page of the documentation, by its path under the site (`using/web/`). */
export const docs = (path: string): string => `${DOCS_URL}/${path}`;
