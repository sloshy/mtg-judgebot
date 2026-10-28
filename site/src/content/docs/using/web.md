---
title: The web app
description: The anonymous interface, with the same pipeline, anonymous ratings, and a rate limit per address.
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

An answer naming a card that is not out yet, and that the loaded Comprehensive Rules may
not cover, opens with the same note as on Discord ([unreleased cards](../discord/#unreleased-cards)).
The feed shows the note on a stored answer as it was when the answer was given.

A reply that is not an answer (busy, rate limited, a card not found, an error, the
maintenance notice) is a titled notice in the judge's message. The header links to this
documentation and has a theme switch (Auto, Dark, Light). The footer names the source
repository, the commit the instance was built from and the operator's support address
(`JUDGE_OPERATOR_EMAIL`, without which the web app does not start). A line above it gives
the Comprehensive Rules release loaded and how long ago the data was last refreshed, for
example `Comprehensive Rules 2026-09-25 · refreshed 3 hours ago`.

Answers are written in the light Markdown Discord renders (bold card names, rule numbers
in backticks). The web app renders the same three marks and nothing else.

A name that could mean several cards gets a "did you mean…?" choice. Questions asked in
one browser session share history, so a follow-up works. Before the first question the
page offers four examples. Choosing one fills the box but does not send it, because an
answer spends the operator's money and one of the visitor's rate-limited questions.

Nobody is logged in there. Nothing about the visitor is stored beyond a random session
id, one per browser tab, that groups their questions and their ratings. Their questions
are listed on the [feed](#the-feed) without a name. The browser itself remembers the
theme, the feed's layout and whether to count web ratings (`localStorage`); none of them
is sent to the server.

## Ratings

An answer in the chat, on the feed and on a call's page has three buttons:
**Correct**, **Partially correct** and **Incorrect**.

- **Correct** is recorded at once.
- **Partially correct** and **Incorrect** open a form under the answer, which the
  keyboard reaches first: the kind of problem (required), *What's wrong* and *The right
  answer* (up to 1000 characters each) and a name (up to 100), all three optional. The
  reason goes to the operator's feedback queue beside the question and answer; the
  judge never reads it.
- Rating an answer the tab already rated asks first, under the buttons: **Replace** or
  **Keep**. *Correct* over *Correct* says it is already recorded.

A web rating is anonymous, so it never shapes which past answers the judge sees as
examples; only Discord's ratings do. Each rating keeps the address and browser it came
from for `FEEDBACK_IP_DAYS` (default 90), for dealing with abuse, then loses both.
Ratings are limited to 30 per address per ten minutes, apart from the questions' limit.

Under each answer on the feed and its page, the counts are Discord's. **Include N from
the web** adds the web's, a choice this browser remembers. The web's are hidden by
default because anyone can inflate them: a new tab is a new rater.

When the operator sets `JUDGE_PUBLIC_FEEDBACK=true`, the reasons raters gave are listed
there too, with the kind of problem and their words, never who gave them; the form says
so before a rater writes one. A reason the operator dismisses in the feedback queue
leaves the public pages. Rating needs `--api` beside `--web`, as asking does.

## The feed

`/feed` lists answered questions from every interface, newest first: Discord, the web app,
the MCP `judge` tool, `judge-cli` and agent sessions. Each entry shows where it was asked,
when, the Discord username of whoever asked (anonymous for the web, the agent tools, calls
from before 2.0.0 and anyone who used `/forget`), the question, the answer with its
citations and cards, how it was rated, and a note when its citations no longer hold.

- Only a question asked with `/judge-private` (or an agent call made private) stays off it.
  A Discord question asked in a channel few people can see is still listed. Servers and
  channels are never shown, and a mention in a question shows as `@…` or `#…`.
- `/feed?interface=discord` (or `web`, `mcp`, `cli`, `agent_session`) lists one
  interface's.
- The search box finds answers whose question, answer or asker's username contains what
  you type (`/feed?q=…`); `@name` lists one Discord user's (`/feed?user=name`).
- **Compact** (the default) shows one line per question, which opens in place to the
  whole exchange; **Expanded** shows every exchange in full.
- `/calls/<id>` is one answer's own page, and `/users/<name>` one Discord user's public
  questions. A username someone gave up and another took lists only the newer holder's
  questions once they ask one publicly; the earlier holder's stay in the feed.
- `JUDGE_FEED=false` removes the pages and their endpoints. `JUDGE_FEED_DAYS` hides calls
  older than that many days from all of them, without deleting anything
  (`JUDGE_CALL_RETENTION_DAYS` deletes).

Turning the feed on lists every earlier public call too, including those asked before it
existed.

## The dashboard

`/dashboard` counts what the feed lists: every question not asked private, within
`JUDGE_FEED_DAYS` when that is set. Private questions are never counted, in any figure.

- The number of questions, of Discord askers, and the share answered.
- Questions per day by interface, as a chart (weeks above three months). **Show as a
  table** lists the same figures as text.
- How the questions went: answered, a "which card?" question, a card not found, out of
  scope, declined by the model, or failed.
- The ratings: correct, partially correct, incorrect. Discord's are counted; **Include N
  from the web** adds the web's, the same choice as on the feed.
- The most-asked topics, cards and Discord askers. A card opens a feed search for it, an
  asker their page, and an interface its part of the feed.
- The range: 7, 30 or 90 days, or a year (`/dashboard?days=90`, or `?from=YYYY-MM-DD&to=…`
  for any range up to 366 days). The interface chips narrow every figure to one.
- `/dashboard?user=name` is one Discord user's figures, linked from their page
  (`/users/name`). Outcomes are not counted per user: a question that was not answered
  records no asker.
- With `JUDGE_PUBLIC_COST=true` it also shows what the answers cost, in all and per
  question. With `JUDGE_PUBLIC_MODELS=true` it shows which models read and answered the
  questions, and narrows to one model. Both are off by default.

The dashboard is there wherever the feed is, and `JUDGE_FEED=false` removes both.

## Enabling

The web app is the `--web` role of `judgebot`. `--api` serves `POST /api/judge`, the route
the web app asks its questions through, and the MCP transport is `--mcp`. Any of the three
also serves `GET /api/health` and `GET /api/about`. The compose service runs
`--discord --api --web --jobs` unless `JUDGE_ROLES` in `.env` says otherwise, so
a `scripts/deploy.sh` deployment serves the web app. `JUDGE_ROLES='--discord --api --jobs'`, for
example, is a deployment with no public page.

While the instance migrates its database for a major release, a question gets a short
notice and a **Try again** button that sends the same question once the upgrade is done.

## Admin pages

The operator's view of the instance is part of the same build, served by the `--admin`
role on a port of its own (`ADMIN_ADDR`, default `127.0.0.1:8789`), never by the public
listener. It needs `ADMIN_TOKEN`; `judge-cli admin-url` prints the page's address with
the token in its `#token=` part. The page keeps the token for the tab and removes it
from the address bar. From another machine, forward the port over SSH, or on a trusted
network publish it on the host's LAN address with a compose override
([Deployment](../../self-hosting/deployment/)).

- **Calls**: every stored call, private ones included, newest first. Filters for the
  interface, the asker (Discord username or id), the model, private or public, and text.
- **A call**: the exchange as the feed shows it, then what is stored with it: who asked
  and where, the thread, every rating with its rater and reason, and the run that
  answered it with each billed request's model, tokens and cost.
- **Feedback**: the feedback queue, open items first when the tab opens. Every
  *Partially correct* or *Incorrect* rating given a reason, newest first, filtered by
  status, kind of problem, interface and rater. An item shows the rater's reason, its
  state, the call whole, and a form that resolves it (with a note and what fixed it) or
  dismisses it (with a note). A rater who replaces their reason reopens it.
- **Failures**: the failed calls, each opening onto the error and what the model sent.
- **Dashboard** (`/admin/usage`): the public dashboard over every call, private ones
  included and counted apart, narrowed by range, interface, asker (Discord username or
  id) and model; then the failed calls, the spend per day and month (chat and embeddings,
  `judgebot ingest` included), the budget period, the cap and what is left (as the
  instance's processes recorded them), the runs by
  outcome, model, interface and stage, the cost per answer, latency, the dearest runs,
  the runs the spend cap refused, the lowest-rated calls, the feedback queue's counts by
  kind of problem and the latest data refreshes. `judge-cli stats` prints the same
  report.

## Limits

Anonymous traffic is rate limited per address (`API_RATE_LIMIT` questions per
`API_RATE_WINDOW_SECS`, default 4 per 5 minutes), ahead of the `JUDGE_CONCURRENCY` slots
and the spend cap. The feed's reads have a window of their own, 120 a minute per address,
so browsing does not use up questions, and the dashboard's another, 30 a minute. The [HTTP API](../../using/api/) page has the request and reply
shapes. Behind a Cloudflare Tunnel, the
[deployment runbook](../../self-hosting/deployment/#5-spend-protection) puts a Cloudflare
rate-limiting rule in front of all of it; behind the bundled public site or your own
proxy, `API_CLIENT_IP=proxy` keeps the count per visitor.

## Developing the web app

```sh
cargo run --release -p judgebot -- --api --web    # API + the built page on localhost:8787
npm --prefix web ci
npm --prefix web run dev             # Vite dev server with /api proxied to :8787
                                     # and /admin/api to an --admin listener on :8789
```
