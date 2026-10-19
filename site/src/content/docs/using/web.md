---
title: The web app
description: The anonymous interface, with the same pipeline, no ratings, and a rate limit per address.
sidebar:
  order: 2
---

The web app is a SolidJS single-page app, built into the image and served by `judgebot` at
<http://localhost:8787>. It reads like a conversation with the judge: your questions on
one side, the judge's answers on the other, and the question box docked at the bottom. It
runs the same pipeline as the Discord bot and shows the same answer:

- the ruling
- its citations, in one section that shows each one's label (a rule, a ruling, Oracle
  text or a prior call) and opens to every verbatim quote, each linked to its source
- the cards the question was resolved to
- the confidence and the CR version.

A reply that is not an answer (busy, rate limited, a card not found, an error, the
maintenance notice) is a titled notice in the judge's message. The header links to this
documentation and has a theme switch (Auto, Dark, Light).

Answers are written in the light Markdown Discord renders (bold card names, rule numbers
in backticks). The web app renders the same three marks and nothing else.

A name that could mean several cards gets a "did you mean…?" choice. Questions asked in
one browser session share history, so a follow-up works. Before the first question the
page offers four examples. Choosing one fills the box but does not send it, because an
answer spends the operator's money and one of the visitor's rate-limited questions.

Nobody is logged in there, so the web app has **no rating buttons**, and nothing about the
visitor is stored beyond a random session id that groups their questions. Their questions
are listed on the [feed](#the-feed) without a name. The browser itself remembers the theme
and the feed's layout (`localStorage`); neither is sent to the server. The footer
names the source repository, the commit the instance was built from and the operator's
support address (`JUDGE_OPERATOR_EMAIL`, without which the web app does not start). A
line above it gives the Comprehensive Rules release loaded and how long ago the data was
last refreshed, for example `Comprehensive Rules 2026-09-25 · refreshed 3 hours ago`.

## The feed

`/feed` lists answered questions from every interface, newest first: Discord, the web app,
the MCP `judge` tool, `judge-cli` and agent sessions. Each entry shows where it was asked,
when, the Discord username of whoever asked (anonymous for the web, the agent tools, calls
from before 2.0.0 and anyone who used `/forget`), the question, the answer with its
citations and cards, how it was rated, and a note when its citations no longer hold.

- Only a question asked with `private: True` (or an agent call made private) stays off it.
  A Discord question asked in a channel few people can see is still listed. Servers and
  channels are never shown, and a mention in a question shows as `@…` or `#…`.
- `/feed?interface=discord` (or `web`, `mcp`, `cli`, `agent_session`) lists one
  interface's.
- The search box finds answers whose question, answer or asker's username contains what
  you type (`/feed?q=…`); `@name` lists one Discord user's (`/feed?user=name`).
- **Compact** (the default) shows one line per question, which opens in place to the
  whole exchange; **Expanded** shows every exchange in full.
- `/calls/<id>` is one answer's own page, and `/users/<name>` one Discord user's public
  questions.
- `JUDGE_FEED=false` removes the pages and their endpoints. `JUDGE_FEED_DAYS` hides calls
  older than that many days from all of them, without deleting anything
  (`JUDGE_CALL_RETENTION_DAYS` deletes).

Turning the feed on lists every earlier public call too, including those asked before it
existed.

## Enabling

The web app is the `--web` role of `judgebot`. `--api` serves `POST /api/judge`, the route
the web app asks its questions through, and the MCP transport is `--mcp`. Any of the three
also serves `GET /api/health` and `GET /api/about`. The compose service runs
`--discord --api --web --jobs` unless `JUDGE_ROLES` in `.env` says otherwise, so
`docker compose up -d` serves the web app. `JUDGE_ROLES='--discord --api --jobs'`, for
example, is a deployment with no public page.

While the instance migrates its database for a major release, a question gets a short
notice and a **Try again** button that sends the same question once the upgrade is done.

## Limits

Anonymous traffic is rate limited per address (`API_RATE_LIMIT` questions per
`API_RATE_WINDOW_SECS`, default 4 per 5 minutes), ahead of the `JUDGE_CONCURRENCY` slots
and the spend cap. The feed's reads have a window of their own, 120 a minute per address,
so browsing does not use up questions. The [HTTP API](../../using/api/) page has the request and reply
shapes, and the [deployment runbook](../../self-hosting/deployment/) puts a Cloudflare
rate-limiting rule in front of all of it.

## Developing the web app

```sh
cargo run --release -p judgebot -- --api --web    # API + the built page on localhost:8787
npm --prefix web ci
npm --prefix web run dev             # Vite dev server with /api proxied to :8787
```
