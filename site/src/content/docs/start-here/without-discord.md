---
title: Try it without Discord
description: Bring up the database, load the data, and ask questions from the web page or the command line. No bot token needed.
sidebar:
  order: 2
---

Nothing in the pipeline depends on Discord. The quickest way to see the judge work is the
web page on your own machine, and the cheapest is the command line. Both need the database
and the data. The model provider is the only paid part.

You need Docker with the compose plugin, Rust 1.98 (`rust-toolchain.toml` makes rustup
install it), and an Anthropic API key. A Voyage AI key adds the semantic-search leg.
Without it the bot runs on the other two.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
cp .env.example .env                                   # set ANTHROPIC_API_KEY (and VOYAGE_API_KEY if you have one)
                                                       # and JUDGE_OPERATOR_EMAIL, which judge-api requires
docker compose up -d db                                # pgvector Postgres on localhost:5432
cargo run --release -p judge-ingest -- migrate         # create the schema
cargo run --release -p judge-ingest -- cards           # Scryfall bulk data (~110 MB, cached in .cache/)
cargo run --release -p judge-ingest -- rules latest    # the current Comprehensive Rules
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed           # optional; a few cents on Voyage
```

The card sync takes a few minutes the first time. The rules parse takes seconds. If port
5432 is already taken on your machine, set `DB_PORT` in `.env` and change the port in
`DATABASE_URL` to match before starting the database.

## The web page

```sh
docker compose pull            # the CI-built image of upstream main, not of your checkout;
                               # skip it and `up` builds what you cloned on first run
                               # instead (minutes, ~4 GB RAM)
docker compose up -d api
```

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
cargo build --release -p judge-agent
target/release/judge-cli judge "does bob's trigger count goyf's mana value as 0?"
target/release/judge-cli card goyf                     # resolve a name, no model call
target/release/judge-cli get-rules 702.19 202.3         # rules text by id
target/release/judge-cli search "mana value of a card with X in its cost"
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
