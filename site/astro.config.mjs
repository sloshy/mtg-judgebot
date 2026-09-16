// @ts-check
import { defineConfig, passthroughImageService } from "astro/config";
import starlight from "@astrojs/starlight";

// GitHub Pages serves the site under the repository name; a custom domain
// (Cloudflare Pages, or Pages with a CNAME) sets SITE_URL and SITE_BASE=/ in the
// build environment instead. publish-docs.yml leaves both at the defaults.
// `astro dev` defaults to base "/" instead, so the local dev server's root and
// links work at http://localhost:4321/ without SITE_BASE having to be set by hand.
// (Passing a function to defineConfig instead of a plain object, to read Astro's
// `command`, breaks Starlight's own integration setup — it ends up injecting no
// pages at all — so the dev/build distinction is read from argv instead.)
const site = process.env.SITE_URL ?? "https://sloshy.github.io";
const isDev = process.argv.includes("dev");

export default defineConfig({
  site,
  base: process.env.SITE_BASE ?? (isDev ? "/" : "/mtg-judgebot"),
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
      // Starlight has no top navigation, so a header link into the docs goes in
      // through the one header component that is overridable (see the file).
      components: { SocialIcons: "./src/components/HeaderLinks.astro" },
      // Every page carries `sidebar.order` in its frontmatter (the synced ones
      // get it from scripts/sync-docs.mjs), so each group autogenerates in a
      // fixed order and adding a page is a frontmatter line, not a config edit.
      sidebar: [
        { label: "Start here", items: [{ autogenerate: { directory: "start-here" } }] },
        { label: "Make your own judgebot", items: [{ autogenerate: { directory: "self-hosting" } }] },
        { label: "Using the judge", items: [{ autogenerate: { directory: "using" } }] },
        { label: "How it works", items: [{ autogenerate: { directory: "how-it-works" } }] },
        { label: "Contributing", items: [{ autogenerate: { directory: "contributing" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "reference" } }] },
      ],
    }),
  ],
});
