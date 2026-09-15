// @ts-check
import { defineConfig, passthroughImageService } from "astro/config";
import starlight from "@astrojs/starlight";

// GitHub Pages serves the site under the repository name; a custom domain
// (Cloudflare Pages, or Pages with a CNAME) sets SITE_URL and SITE_BASE=/ in the
// build environment instead. publish-docs.yml leaves both at the defaults.
const site = process.env.SITE_URL ?? "https://sloshy.github.io";
const base = process.env.SITE_BASE ?? "/mtg-judgebot";

export default defineConfig({
  site,
  base,
  // No images to optimise, so no sharp: the site builds anywhere Node runs.
  image: { service: passthroughImageService() },
  integrations: [
    starlight({
      title: "MTG Judgebot",
      description:
        "A Discord bot and web page that answers Magic: The Gathering rules questions with validated citations.",
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/sloshy/mtg-judgebot" },
      ],
      editLink: {
        baseUrl: "https://github.com/sloshy/mtg-judgebot/edit/main/site/",
      },
      customCss: ["./src/styles/custom.css"],
      // Every page carries `sidebar.order` in its frontmatter (the synced ones
      // get it from scripts/sync-docs.mjs), so each group autogenerates in a
      // fixed order and adding a page is a frontmatter line, not a config edit.
      sidebar: [
        { label: "Start here", items: [{ autogenerate: { directory: "start-here" } }] },
        { label: "Self-hosting", items: [{ autogenerate: { directory: "self-hosting" } }] },
        { label: "Using the judge", items: [{ autogenerate: { directory: "using" } }] },
        { label: "How it works", items: [{ autogenerate: { directory: "how-it-works" } }] },
        { label: "Contributing", items: [{ autogenerate: { directory: "contributing" } }] },
        { label: "Design history", items: [{ autogenerate: { directory: "design-history" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "reference" } }] },
      ],
    }),
  ],
});
