---
title: Try it without Discord
description: Bring up the database, load the data, and ask questions from the web app or the command line. No bot token needed.
sidebar:
  order: 2
---

Nothing in the pipeline depends on Discord. The quickest way to see the judge work is the
web app on your own machine, and the cheapest is the command line. Both need the database
and the data. The model provider is the only paid part.

You need Docker with the compose plugin and an Anthropic API key. A Voyage AI key is
optional. It adds a semantic-search source to retrieval. Without it the judge uses the
other two sources: the rules for the question's category, and full-text search. Nothing is compiled: the image is published for amd64 and arm64.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
docker compose pull                     # the published image
scripts/config.sh                       # the config editor: open the URL it prints, set ANTHROPIC_API_KEY
                                        # (and VOYAGE_API_KEY if you have one), JUDGE_OPERATOR_EMAIL,
                                        # which the web app requires, and JUDGE_ROLES to '--api --web --jobs'
                                        # (no Discord); save, then Ctrl-C
docker compose up -d db                 # pgvector Postgres on localhost:5432
docker compose run --rm refresh init    # the whole first load, in one command
```

`init` runs seven steps and logs each with its time:

- `migrate`: the schema.
- `cards`: Scryfall's bulk data, about 110 MB.
- `rules latest`: the current Comprehensive Rules.
- `aliases` and `notes`: the nickname and difficult-card lists built into the binary.
- `embed`: a few cents on Voyage. Skipped with a warning when no embedder is configured.
- `emoji`: skipped until there is a `DISCORD_TOKEN`.

Without embeddings it takes about a minute on a fast connection, nearly all of it the
Scryfall download. It stops at the first failure. Every step is idempotent, so the fix for a
failed `init` is to run it again.

If port 5432 is already taken on your machine, set `DB_PORT` and change the port in
`DATABASE_URL` to match before starting the database.

The [config editor](../../self-hosting/config-editor/) checks each change with the
binaries' own loaders. To edit by hand instead, `cp .env.example .env` and fill it in.

## The web app

```sh
docker compose up -d
```

This starts `judgebot` with the roles `JUDGE_ROLES` names: the web app, the question route
and the daily data refresh. Without `JUDGE_ROLES` it would also start the Discord bot, and
refuse to start for want of a `DISCORD_TOKEN`.

The image you pulled is CI's build of upstream `main`, not of your checkout. To run what
you cloned or changed, use `docker compose up -d --build` instead (minutes, about
4 GB of RAM).

Open <http://localhost:8787>. The web app runs the same pipeline as the bot, without the
rating buttons.

By default each address may ask 4 questions per 5 minutes. That suits a public page but
will stop you quickly while testing. To ask more, raise `API_RATE_LIMIT` (or shorten
`API_RATE_WINDOW_SECS`) in `.env` first.

Each role is a launch option. `--api --jobs` alone serves the question route with no
public web app, and `--mcp` adds the MCP transport.

To develop the API without Docker, run `cargo run --release -p judgebot -- --api --web`.
It serves the built page from `web/dist` (run
`npm --prefix web ci && npm --prefix web run build` once). `npm --prefix web run dev` runs
Vite with `/api` proxied to it.

## The command line

`judge-cli` runs the same pipeline from a shell and prints JSON:

```sh
alias judge-cli='docker compose run --rm --entrypoint judge-cli judgebot'   # from the repository directory
judge-cli judge "does bob's trigger count goyf's mana value as 0?"
judge-cli card goyf                                    # resolve a name, no model call
judge-cli get-rules 702.19 202.3         # rules text by id
judge-cli search "mana value of a card with X in its cost"
```

`judge` spends model budget under its own `JUDGE_MAX_USD`. The lookup commands are free.
The [agents page](../../using/agents/) covers the session mode, in which an outside
agent (or you) does the model's job and the pipeline only validates.

## Cost

A typical answer costs $0.07 to $0.15 in model calls. Every call is metered against
`JUDGE_MAX_USD` (default $5 per process), which is roughly 30 to 65 answers. The cap reserves
the worst case before sending, so a process cannot overshoot it.

By default the cap is a lifetime total for one process, not a budget per day or month.
The compose deployment's `judgebot` is one process, so the bot and the web app share it, and
a restart starts again from zero.

`JUDGE_BUDGET_PERIOD=day` or `month` makes it a budget instead. There is then one cap for
the current UTC day or month. Every process shares it through the database, it survives
restarts, and it resets when the next period starts.

A call is refused once the money left under the cap is less than its worst-case cost.
Synthesis therefore stops about $0.36 short of the cap, and from then on nothing is
answered. Discord members are told the bot has hit its spending cap. A model priced
`free` is never refused.

The log shows `judge failed` with `spend cap exceeded: spent $… of $… cap`, then `spend
cap reached`. Set `JUDGE_ALERT_WEBHOOK` to be told in a Discord or Slack channel. Run
`judge-cli stats` for spend and questions per day.
