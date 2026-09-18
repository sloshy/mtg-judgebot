---
title: Requirements and first run
description: What your own judgebot needs, in what order to bring it up, and what each step costs.
sidebar:
  order: 1
---

The community that uses a judgebot runs it, with its own Discord application, model key
and spend cap. A complete instance is three containers from one compose file: `db`
(Postgres with pgvector), `api` (the web page and HTTP API) and `bot` (Discord). A fourth,
`refresh`, runs nightly from cron to keep cards and rules current. An optional
`cloudflared` publishes the API without an open port. You can stop after `api` and never
touch Discord. The bot is the last thing to add.

## Requirements

- A host that stays on, with Docker and the compose plugin. Running takes about 200 MB of
  RAM across the three containers. **Building** the image takes ~4 GB and a lot of CPU. A
  low-powered host pulls the CI-built image instead (`docker compose pull`).
- Rust 1.97 on the machine where you run the data loads. Or run them inside the image
  (`docker compose run --rm refresh cards`, and so on). `refresh` is the `judge-ingest`
  binary.
- A model provider. With `.env` alone that is Anthropic's API (`ANTHROPIC_API_KEY`).
  A [`judge.toml`](../../self-hosting/models/) chooses anything else.
- Optionally a Voyage AI key for the semantic-search leg, or an embedding model on an
  OpenAI-compatible server. Without one the bot runs on the category map and full-text
  legs and logs a warning at startup.
- For Discord, an application of your own and its bot token.
  [Create the Discord app](../../self-hosting/discord-app/) walks through the developer
  portal with links into Discord's documentation.

## Order of operations

1. `cp .env.example .env` and fill in the keys. Every knob is documented in the file and in
   the [configuration reference](../../self-hosting/configuration/).
2. `docker compose up -d db`. Postgres publishes on **localhost:5432** (loopback only). If
   something on the host already has that port, set `DB_PORT` in `.env` and change the
   port in `DATABASE_URL` to match.
3. Load the data: `migrate`, `cards`, `rules latest`, `aliases`, `notes`, and `embed` if you
   have an embedder, as on the [Try it without Discord](../../start-here/without-discord/)
   page. The first embed pays the embedder once for every rule and glossary entry, a few
   cents on Voyage. Afterwards only changed rules are re-embedded.
4. `docker compose up -d api`. Open <http://localhost:8787> and ask a question. This runs
   the full pipeline, so it is a good place to check the model setup before Discord is
   involved.
   `api` and `bot` both apply pending migrations at startup unless
   `JUDGE_AUTO_MIGRATE=false`.
5. [Create the Discord app](../../self-hosting/discord-app/), then `docker compose up -d
   bot`. Then run `cargo run --release -p judge-ingest -- emoji` once, so answers show mana
   symbols as pictures instead of `{W}`.
6. Schedule `scripts/refresh-data.sh` nightly and `scripts/backup-db.sh` weekly. The
   [deployment runbook](../../self-hosting/deployment/) has the cron lines.

## Embedding width

The schema is created with `vector(1024)` columns, Voyage's width. If your embedder
produces another width (OpenAI's `text-embedding-3-small` is 1536), run
`judge-ingest reembed --yes` instead of `embed` the first time. It retypes the columns and
records the embedding space before filling them. The bot refuses to mix two spaces. On a
mismatch it logs an error naming both and runs with the vector leg dark.

## Behind a domain

The deployment runbook publishes `api` through a Cloudflare Tunnel with edge rate
limiting in front of the anonymous page. Any reverse proxy works. Keep
`API_CLIENT_IP=peer` unless Cloudflare is the only route to the origin. The
[security page](../../reference/security/) gives the reason.
