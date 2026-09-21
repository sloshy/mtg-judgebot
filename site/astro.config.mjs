// @ts-check

import starlight from "@astrojs/starlight";
import { defineConfig, passthroughImageService } from "astro/config";

// The site is served at the root of its own domain (GitHub Pages with a custom
// domain). A build for somewhere else sets SITE_URL, and SITE_BASE when it is
// served under a path (a fork's <owner>.github.io/<repo> needs SITE_BASE=/<repo>).
const site = process.env.SITE_URL ?? "https://mtg-judgebot.rpeters.dev";
const base = process.env.SITE_BASE ?? "/";
// A path under the base, whether SITE_BASE came with its slashes or not.
const inBase = (/** @type {string} */ path) =>
  `/${base.replace(/^\/|\/$/g, "")}${path}`.replace(/^\/\//, "/");

export default defineConfig({
  site,
  base,
  // The one image is pixel art served as is, so no sharp: the site builds
  // anywhere Node runs.
  image: { service: passthroughImageService() },
  integrations: [
    starlight({
      title: "MTG Judgebot",
      description:
        "A Discord bot and web page that answers Magic: The Gathering rules questions with validated citations.",
      // The icon is 32x32 pixel art (assets/icon.png at the repository root is the
      // 512px original). The favicon is that grid at native size, so a tab never
      // smooths it, and custom.css keeps the header logo's pixels hard.
      logo: { src: "./src/assets/icon.png", alt: "" },
      favicon: "/favicon.png",
      head: [
        { tag: "link", attrs: { rel: "apple-touch-icon", href: inBase("/apple-touch-icon.png") } },
        {
          tag: "meta",
          attrs: { property: "og:image", content: new URL(inBase("/icon.png"), site).href },
        },
      ],
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/sloshy/mtg-judgebot" }],
      editLink: {
        baseUrl: "https://github.com/sloshy/mtg-judgebot/edit/main/site/",
      },
      customCss: ["./src/styles/custom.css"],
      // Starlight has no top navigation, so a header link into the docs goes in
      // through the one header component that is overridable (see the file).
      // The footer adds the Fan Content Policy statement and the data sources to
      // Starlight's own, on every page including the splash ones.
      components: {
        SocialIcons: "./src/components/HeaderLinks.astro",
        Footer: "./src/components/Footer.astro",
      },
      // Every page carries `sidebar.order` in its frontmatter (the synced ones
      // get it from scripts/sync-docs.mjs), so each group autogenerates in a
      // fixed order and adding a page is a frontmatter line, not a config edit.
      sidebar: [
        { label: "Start here", items: [{ autogenerate: { directory: "start-here" } }] },
        {
          label: "Run your own judgebot",
          items: [{ autogenerate: { directory: "self-hosting" } }],
        },
        { label: "Using the judge", items: [{ autogenerate: { directory: "using" } }] },
        { label: "How it works", items: [{ autogenerate: { directory: "how-it-works" } }] },
        { label: "Contributing", items: [{ autogenerate: { directory: "contributing" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "reference" } }] },
      ],
    }),
  ],
});
