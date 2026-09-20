---
title: Try it without Discord
description: Bring up the database, load the data, and ask questions from the web page or the command line. No bot token needed.
sidebar:
  order: 2
---

Nothing in the pipeline depends on Discord. The quickest way to see the judge work is the
web page on your own machine, and the cheapest is the command line. Both need the database
and the data. The model provider is the only paid part.

You need Docker with the compose plugin and an Anthropic API key. A Voyage AI key adds
the semantic-search leg. Without it the judge runs on the other two. Nothing is compiled:
the image is published for amd64 and arm64.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
cp .env.example .env                    # set ANTHROPIC_API_KEY (and VOYAGE_API_KEY if you have one)
                                        # and JUDGE_OPERATOR_EMAIL, which judge-api requires
docker compose pull                     # the published image
docker compose up -d db                 # pgvector Postgres on localhost:5432
docker compose run --rm refresh init    # the whole first load, in one command
```

`init` runs seven steps and logs each with its time: `migrate` (the schema), `cards`
(Scryfall's bulk data, about 110 MB), `rules latest` (the current Comprehensive Rules),
`aliases` and `notes` (the nickname and difficult-card lists built into the binary),
`embed` (skipped with a warning when no embedder is configured, a few cents on Voyage
otherwise) and `emoji` (skipped until there is a `DISCORD_TOKEN`). Without embeddings it
takes about a minute on a fast connection, nearly all of it the Scryfall download. It stops at the first failure, every step is idempotent, and
the fix for a failed `init` is to run it again.

If port 5432 is already taken on your machine, set `DB_PORT` in `.env` and change the port
in `DATABASE_URL` to match before starting the database.

## The web page

```sh
docker compose up -d api
```

The image you pulled is CI's build of upstream `main`, not of your checkout. To run what
you cloned or changed, use `docker compose up -d --build api` instead (minutes, about
4 GB of RAM).

Open <http://localhost:8787>. The anonymous API allows each address 4 questions per 5
minutes by default, which suits a public page and will stop you within minutes of testing
your own. Raise `API_RATE_LIMIT` (or shorten `API_RATE_WINDOW_SECS`) in `.env` first if you
plan to ask more. The page is the same pipeline the bot runs, minus rating buttons. Every
front door `judge-api` has is a launch option. The compose file passes `--api --web`, and
`API_INTERFACES` in `.env` changes that list. `--api` alone runs the question route with no
public page.

To develop the API without Docker, run `cargo run --release -p judge-api -- --api --web`.
It serves the built page from `web/dist` (run
`npm --prefix web ci && npm --prefix web run build` once). `npm --prefix web run dev` runs
Vite with `/api` proxied to it.

## The command line

`judge-cli` runs the same pipeline from a shell and prints JSON:

```sh
alias judge-cli='docker compose run --rm --entrypoint judge-cli api'   # from the repository directory
judge-cli judge "does bob's trigger count goyf's mana value as 0?"
judge-cli card goyf                                    # resolve a name, no model call
judge-cli get-rules 702.19 202.3         # rules text by id
judge-cli search "mana value of a card with X in its cost"
```

`judge` spends model budget under its own `JUDGE_MAX_USD`. The lookup commands are free.
The [agents page](../../using/agents/) covers the session mode, in which an outside
agent (or you) does the model's job and the pipeline only validates.

## Cost

A typical answer costs $0.08 to $0.25 in model calls. Every call is metered against
`JUDGE_MAX_USD` (default $5 per process). The cap reserves the worst case before
sending, so a process cannot overshoot it.

By default the cap is a lifetime total for one process, not a budget per day or month.
`bot` and `api` are separate processes with a cap each, so a compose deployment can spend
twice `JUDGE_MAX_USD`, and a restart starts again from zero. `JUDGE_BUDGET_PERIOD=day` or
`month` makes it a budget instead: one cap for the current UTC day or month, shared by
`bot` and `api` through the database, kept across restarts, and lifted by itself when the
next period starts.

A call is refused once the headroom left is smaller than its worst case, so synthesis
stops about $0.45 short of the cap, and from there nothing is answered (Discord members
are told the bot has hit its spending cap). A model priced `free` is never refused. The
log shows `judge failed` with `spend cap exceeded: spent $… of $… cap`, then `spend cap
reached`. Set `JUDGE_ALERT_WEBHOOK` to be told in a Discord or Slack channel, and run
`judge-cli stats` for spend and questions per day.
At that cost per answer, $5 is roughly 20 to 55 answers.
