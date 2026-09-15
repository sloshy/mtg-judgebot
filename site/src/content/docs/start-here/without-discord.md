---
title: Try it without Discord
description: Bring up the database, load the data, and ask questions from the web page or the command line. No bot token needed.
sidebar:
  order: 2
---

Nothing in the pipeline depends on Discord. The quickest way to see the judge work is the
web page on your own machine, and the cheapest is the command line. Both need the database
and the data; the model provider is the only paid part.

You need Docker with the compose plugin, Rust 1.97 (`rust-toolchain.toml` makes rustup
install it), and an Anthropic API key. A Voyage AI key adds the semantic-search leg; skip
it and the bot runs on the other two.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
cp .env.example .env                                   # set ANTHROPIC_API_KEY (and VOYAGE_API_KEY if you have one)
docker compose up -d db                                # pgvector Postgres on localhost:5432
cargo run --release -p judge-ingest -- migrate         # create the schema
cargo run --release -p judge-ingest -- cards           # Scryfall bulk data (~110 MB, cached in .cache/)
cargo run --release -p judge-ingest -- rules latest    # the current Comprehensive Rules
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed           # optional; a few cents on Voyage
```

The card sync takes a few minutes the first time; the rules parse is seconds. If port
5432 is already taken on your machine, set `DB_PORT` in `.env` and change the port in
`DATABASE_URL` to match before starting the database.

## The web page

```sh
docker compose up -d api       # builds the image on first run (minutes, ~4 GB RAM)
```

Open <http://localhost:8787>. The page is the same pipeline the bot runs, minus rating
buttons. `API_RATE_LIMIT` and `API_RATE_WINDOW_SECS` in `.env` govern how many questions an
IP can ask; the defaults are for a public instance, so raise them for yourself.

For development without Docker for the API: `cargo run --release -p judge-api` serves the
built page from `web/dist` (`npm --prefix web ci && npm --prefix web run build` once), and
`npm --prefix web run dev` runs Vite with `/api` proxied to it.

## The command line

`judge-cli` runs the same pipeline from a shell and prints JSON:

```sh
cargo build --release -p judge-agent
target/release/judge-cli judge "does bob's trigger count goyf's mana value as 0?"
target/release/judge-cli card goyf                     # resolve a name, no model call
target/release/judge-cli get-rules 702.19 202.3         # rules text by id
target/release/judge-cli search "mana value of a card with X in its cost"
```

`judge` spends model budget under its own `JUDGE_MAX_USD`; the lookup commands are free.
The [agents page](../../using/agents/) covers the session mode, in which an outside
agent (or you) does the model's job and the pipeline only validates.

## Cost

A typical answer costs $0.08 to $0.25 in model calls. Every call is metered against
`JUDGE_MAX_USD` (default $5 per process), and the cap reserves the worst case before
sending, so a process can never overshoot it.
