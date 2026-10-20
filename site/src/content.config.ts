import { defineCollection } from "astro:content";
import { docsLoader, i18nLoader } from "@astrojs/starlight/loaders";
import { docsSchema, i18nSchema } from "@astrojs/starlight/schema";

export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  // Declared so Starlight does not warn about it. Starlight's own interface strings
  // come with it for every locale; the site's components keep theirs in src/i18n.ts.
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};
