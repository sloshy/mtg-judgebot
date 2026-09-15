// Copies the repository's canonical Markdown (docs/*.md, CONTRIBUTING.md, …) into
// the Starlight content tree with the frontmatter Starlight needs. The sources
// stay where the code, CLAUDE.md and the README already point; the copies are
// build output and are gitignored (this script writes that .gitignore too).
//
// A manifest entry copies one file, or one range of its numbered `## N.`
// sections, and may strip the first H1 (Starlight renders `title`). Nothing
// else is rewritten: the docs refer to files in backticks, not links.

import { mkdirSync, readFileSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const out = resolve(here, "..", "src", "content", "docs");

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
    description: "Where the rules, cards and rulings come from; the three retrieval legs; the synthesis guardrails.",
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
    src: "docs/proposals/providers.md", dest: "how-it-works/providers.md",
    title: "Providers and the model seam", order: 6,
    description: "The judge.toml provider model: backends, endpoints, dialects, pricing and the embedding space. Sections 4 and 5 are the normative reference.",
  },
  {
    src: "README.md", dest: "how-it-works/evaluation.md",
    title: "Evaluation", order: 7, between: ["## Evaluation", "## Design"],
    description: "The gold set, the free retrieval gate, and the paid full run.",
  },
  // Self-hosting
  {
    src: "README.md", dest: "self-hosting/models.md",
    title: "Choosing a model (judge.toml)", order: 4, between: ["### Choosing a model", "### The web page"],
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
    description: "The anonymous front door: same pipeline, no ratings, rate limited per IP.",
  },
  // Contributing
  {
    src: "CONTRIBUTING.md", dest: "contributing/development.md",
    title: "Development setup and gates", order: 1,
    description: "Environment, the CI gates, SQL and prompt change procedures, and the design rules.",
  },
  // Design history
  {
    src: "docs/LANGUAGE_EVALUATION.md", dest: "design-history/language-evaluation.md",
    title: "Why Rust", order: 1, badge: "Historical",
    description: "The 2026-08-29 language comparison that chose Rust over Scala 3 and TypeScript.",
  },
  {
    src: "docs/proposals/rust.md", dest: "design-history/proposal-rust.md",
    title: "Proposal C: Rust (selected)", order: 2, badge: "Historical",
  },
  {
    src: "docs/proposals/scala.md", dest: "design-history/proposal-scala.md",
    title: "Proposal A: Scala 3", order: 3, badge: "Historical",
  },
  {
    src: "docs/proposals/typescript.md", dest: "design-history/proposal-typescript.md",
    title: "Proposal B: TypeScript", order: 4, badge: "Historical",
  },
  {
    src: "docs/proposals/tenancy.md", dest: "design-history/proposal-tenancy.md",
    title: "Proposal E: Multi-server tenancy", order: 5, badge: "Proposed",
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

const historical =
  "> **Design history.** This document records a decision as it was made and is kept " +
  "for the reasoning, not maintained as a description of the current code. " +
  "*Architecture* and the *Explainer* describe what exists today.\n\n";

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
  fm.push(`editUrl: ${JSON.stringify("https://github.com/sloshy/mtg-judgebot/edit/main/" + e.src)}`);
  return `---\n${fm.join("\n")}\n---\n\n`;
}

const written = [];
for (const e of manifest) {
  const raw = readFileSync(join(repo, e.src), "utf8");
  let body = e.sections ? sections(raw, e.sections) : e.between ? between(raw, e.between) : stripH1(raw);
  if (e.lead) body = `*${e.lead}*\n\n${body}`;
  if (e.badge === "Historical") body = historical + body;
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
