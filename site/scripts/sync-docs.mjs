// Copies the repository's canonical Markdown (docs/*.md, CONTRIBUTING.md, …) into
// the Starlight content tree with the frontmatter Starlight needs. The sources
// stay where the code, CLAUDE.md and the README already point; the copies are
// build output and are gitignored (this script writes that .gitignore too).
//
// A manifest entry copies one file, or one range of its numbered `## N.`
// sections, and may strip the first H1 (Starlight renders `title`). Nothing
// else is rewritten: the docs refer to files in backticks, not links.

import { mkdirSync, readFileSync, writeFileSync, rmSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const out = resolve(here, "..", "src", "content", "docs");
// Copies point their edit link at the canonical file, not at themselves.
const editBase = "https://github.com/sloshy/mtg-judgebot/edit/main/";

/** @type {Array<{src: string, dest: string, title: string, description?: string, order: number, sections?: [number, number], badge?: string, lead?: string}>} */
const manifest = [
  // How it works
  {
    src: "docs/EXPLAINER.md", dest: "how-it-works/explainer-1-what-and-why.md",
    title: "Explainer 1: What it does and why", order: 1, sections: [1, 3],
    description: "What the judge does, why a bare model is not enough, and one question end to end.",
    lead: "The first of four parts of `docs/EXPLAINER.md`, an end-to-end tour for a programmer new to retrieval-augmented systems.",
  },
  {
    src: "docs/EXPLAINER.md", dest: "how-it-works/explainer-2-data-and-retrieval.md",
    title: "Explainer 2: Data and retrieval", order: 2, sections: [4, 6],
    description: "Data sources for rules, cards and rulings, the three retrieval legs, and the synthesis guardrails.",
    lead: "Part two of the explainer. Part one covered the pipeline end to end.",
  },
  {
    src: "docs/EXPLAINER.md", dest: "how-it-works/explainer-3-failure-feedback-money.md",
    title: "Explainer 3: Failure, feedback and money", order: 3, sections: [7, 10],
    description: "What goes wrong and what handles it, the rating loop, the spend cap, and the three front doors.",
    lead: "Part three of the explainer.",
  },
  {
    src: "docs/EXPLAINER.md", dest: "how-it-works/explainer-4-providers-eval-technology.md",
    title: "Explainer 4: Providers, evaluation, technology", order: 4, sections: [11, 15],
    description: "Models as configuration, the gold-set evaluation, what each technology buys, and a glossary.",
    lead: "The last part of the explainer.",
  },
  {
    src: "docs/ARCHITECTURE.md", dest: "how-it-works/architecture.md",
    title: "Architecture", order: 5,
    description: "The design reference: pipeline stages, data, domain model.",
  },
  {
    src: "docs/DECISIONS.md", dest: "how-it-works/decisions.md",
    title: "Design decisions", order: 6,
    description: "Each main design decision, why it was made, and the alternative it rejected.",
  },
  {
    src: "docs/PROVIDERS.md", dest: "how-it-works/providers.md",
    title: "Model providers", order: 7,
    description: "The judge.toml provider model, covering the seam, the backends and their doors, dialect knobs, pricing and the embedding space.",
  },
  {
    src: "README.md", dest: "how-it-works/evaluation.md",
    title: "Evaluation", order: 8, between: ["## Evaluation", "## Design"],
    description: "The gold set, the free retrieval gate, and the paid full run.",
  },
  // Self-hosting
  {
    src: "README.md", dest: "self-hosting/models.md",
    title: "Model choice (judge.toml)", order: 4, between: ["### Choosing a model", "### The web page"],
    description: "Providers, models per stage, pricing for the spend cap, and embeddings.",
  },
  {
    src: "docs/DEPLOYMENT.md", dest: "self-hosting/deployment.md",
    title: "Production deployment", order: 5,
    description: "The runbook: Cloudflare Tunnel, backups to R2, the nightly refresh, redeploying and rolling back.",
  },
  // Using
  {
    src: "README.md", dest: "using/web.md",
    title: "The web page", order: 2, between: ["### The web page", "### Hosting"],
    description: "The anonymous front door, with the same pipeline, no ratings, and a rate limit per IP.",
  },
  // Contributing
  {
    src: "CONTRIBUTING.md", dest: "contributing/development.md",
    title: "Development setup and gates", order: 1,
    description: "Environment, the CI gates, SQL and prompt change procedures, and the design rules.",
  },
  // Reference
  {
    src: "CHANGELOG.md", dest: "reference/changelog.md",
    title: "Changelog", order: 3,
  },
  {
    src: "SECURITY.md", dest: "reference/security.md",
    title: "Security", order: 4,
  },
];

function sections(text, [from, to]) {
  const lines = text.split("\n");
  const isH2 = (l) => /^## (\d+)\./.exec(l);
  let start = -1, end = lines.length;
  for (let i = 0; i < lines.length; i++) {
    const m = isH2(lines[i]);
    if (!m) continue;
    const n = Number(m[1]);
    if (n === from && start < 0) start = i;
    if (n === to + 1) { end = i; break; }
  }
  if (start < 0) throw new Error(`section ${from} not found`);
  return lines.slice(start, end).join("\n");
}

function between(text, [from, to]) {
  const lines = text.split("\n");
  const start = lines.findIndex((l) => l.trim() === from);
  if (start < 0) throw new Error(`heading ${JSON.stringify(from)} not found`);
  let end = lines.findIndex((l, i) => i > start && l.trim() === to);
  if (end < 0) end = lines.length;
  // Drop the heading itself: the page title carries it.
  return lines.slice(start + 1, end).join("\n");
}

function stripH1(text) {
  return text.replace(/^﻿?# [^\n]*\n+/, "");
}

function frontmatter(e) {
  const fm = [`title: ${JSON.stringify(e.title)}`];
  if (e.description) fm.push(`description: ${JSON.stringify(e.description)}`);
  const sidebar = [`  order: ${e.order}`];
  if (e.badge) sidebar.push(`  badge:`, `    text: ${JSON.stringify(e.badge)}`, `    variant: note`);
  fm.push("sidebar:", ...sidebar);
  fm.push(`editUrl: ${JSON.stringify(editBase + e.src)}`);
  return `---\n${fm.join("\n")}\n---\n\n`;
}

// Copies whose manifest entry is gone would otherwise stay in the tree (and be
// built) forever. A copy is recognisable by the frontmatter this script writes —
// an `editUrl` pointing at a repository file — so sweep those, never an authored
// page. (The .gitignore below is tracked, so it cannot serve as the list: a pull
// rewrites it before this runs.)
// JSON.stringify closes the quote; drop it so the marker is a prefix of every copy's editUrl.
const marker = `editUrl: ${JSON.stringify(editBase).slice(0, -1)}`;
const inManifest = new Set(manifest.map((e) => e.dest));
for (const file of readdirSync(out, { recursive: true, withFileTypes: true })) {
  if (!file.isFile() || !file.name.endsWith(".md")) continue;
  const rel = join(file.parentPath ?? file.path, file.name).slice(out.length + 1);
  if (inManifest.has(rel)) continue;
  const head = readFileSync(join(out, rel), "utf8").slice(0, 2000);
  if (head.includes(marker)) rmSync(join(out, rel), { force: true });
}

const written = [];
for (const e of manifest) {
  const raw = readFileSync(join(repo, e.src), "utf8");
  let body = e.sections ? sections(raw, e.sections) : e.between ? between(raw, e.between) : stripH1(raw);
  if (e.lead) body = `*${e.lead}*\n\n${body}`;
  const dest = join(out, e.dest);
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, frontmatter(e) + body.trimEnd() + "\n");
  written.push(e.dest);
}
// Synced copies are build output.
writeFileSync(
  join(out, ".gitignore"),
  "# Written by scripts/sync-docs.mjs from the repository's canonical Markdown.\n" +
    written.map((w) => "/" + w).join("\n") + "\n",
);
console.log(`synced ${written.length} pages into src/content/docs`);
