---
title: The web page
description: The anonymous front door, with the same pipeline, no ratings, and a rate limit per address.
sidebar:
  order: 2
---

The web page is a single SolidJS page, built into the image and served by `judge-api` at
<http://localhost:8787>. It runs the same pipeline as the Discord bot and shows the same
answer: the ruling, a citation per line linked to its source, the cards the question was
resolved to, the confidence and the CR version. A name that could mean several cards
gets a "did you mean…?" choice. Questions asked in one browser session share history, so
a follow-up works. Before the first question the page offers four examples. Choosing one
fills the box and leaves sending it to the visitor, because an answer spends the
operator's money and one of the visitor's rate-limited questions.

Nobody is logged in there, so the page has **no rating buttons**, and nothing about the
visitor is stored beyond a random session id that groups their questions. The footer
names the source repository, the commit the instance was built from and the operator's
support address (`JUDGE_OPERATOR_EMAIL`, which `judge-api` does not start without).

## Turning it on and off

Each of `judge-api`'s front doors is a launch option. Run on its own it serves
`POST /api/judge` and nothing else. The page needs `--web`, and the MCP transport needs
`--mcp`. The compose file passes `--api --web`, so `docker compose up -d api` serves the
page. Set `API_INTERFACES` in `.env` to change that list, for example `--api` alone for a
deployment with no public page.

## Limits

Anonymous traffic is rate limited per address (`API_RATE_LIMIT` questions per
`API_RATE_WINDOW_SECS`, default 4 per 5 minutes), ahead of the `JUDGE_CONCURRENCY` slots
and the spend cap. The [HTTP API](../../using/api/) page has the request and reply
shapes, and the [deployment runbook](../../self-hosting/deployment/) puts a Cloudflare
rate-limiting rule in front of all of it.

## Developing the page

```sh
cargo run --release -p judge-api -- --api --web   # API + the built page on localhost:8787
npm --prefix web ci
npm --prefix web run dev             # Vite dev server with /api proxied to :8787
```
