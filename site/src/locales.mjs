// @ts-check
// The site's languages, the one list: Starlight's `locales` (astro.config.mjs),
// the manifest's per-locale entries (scripts/sync-docs.mjs) and the header and
// footer strings (src/i18n.ts) all read it. English is the root locale, served
// at unprefixed URLs; another language is a key naming its URL directory
// (`es: { label: "Español", lang: "es" }` serves /es/…). Only English ships.

export const locales = /** @type {const} */ ({
  root: { label: "English", lang: "en" },
});
