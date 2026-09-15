---
title: Requirements and first run
description: What a self-hosted instance needs, in what order to bring it up, and what each step costs.
sidebar:
  order: 1
---

A complete instance is three containers from one compose file: `db` (Postgres with
pgvector), `api` (the web page and HTTP API) and `bot` (Discord). A fourth, `refresh`,
runs nightly from cron to keep cards and rules current, and an optional `cloudflared`
publishes the API without an open port.

## Requirements

- A host that stays on, with Docker and the compose plugin. Running takes about 200 MB of
  RAM across the three containers. **Building** the image takes ~4 GB and real CPU; a
  low-powered host pulls the CI-built image instead (`docker compose pull`).
- Rust 1.97 on the machine where you run the data loads, or run them inside the image
  (`docker compose run --rm refresh cards`, and so on: `refresh` is the `judge-ingest`
  binary).
- A model provider. With nothing but `.env` that is Anthropic's API (`ANTHROPIC_API_KEY`);
  a [`judge.toml`](../../self-hosting/models/) chooses anything else.
- Optionally a Voyage AI key for the semantic-search leg, or an embedding model on an
  OpenAI-compatible server. Without one the bot runs on the category map and full-text
  legs and logs a warning at startup.
- For Discord, an application and bot token: [Create the Discord app](../../self-hosting/discord-app/).

## Order of operations

1. `cp .env.example .env` and fill in the keys. Every knob is documented in the file and in
   the [configuration reference](../../self-hosting/configuration/).
2. `docker compose up -d db`. Postgres publishes on **localhost:5433** so it never collides
   with a Postgres already on the host; change `DATABASE_URL` and the port mapping together
   if you must move it.
3. Load the data: `migrate`, `cards`, `rules latest`, `aliases`, `notes`, and `embed` if you
   have an embedder, as on the [Try it without Discord](../../start-here/without-discord/)
   page. The first embed pays the embedder once for every rule and glossary entry, a few
   cents on Voyage; afterwards only changed rules are re-embedded.
4. `docker compose up -d api bot` (or just `api`). Both apply pending migrations at startup
   unless `JUDGE_AUTO_MIGRATE=false`.
5. Once, `cargo run --release -p judge-ingest -- emoji` uploads the mana and card symbols as
   application emoji so answers on Discord show pictures instead of `{W}`.
6. Schedule `scripts/refresh-data.sh` nightly and `scripts/backup-db.sh` weekly. The
   [deployment runbook](../../self-hosting/deployment/) has the cron lines.

## Embedding width

The schema is created with `vector(1024)` columns, Voyage's width. If your embedder
produces another width (OpenAI's `text-embedding-3-small` is 1536), run
`judge-ingest reembed --yes` instead of `embed` the first time; it retypes the columns and
records the embedding space before filling them. The bot refuses to mix two spaces: on a
mismatch it logs an error naming both and runs with the vector leg dark.

## Behind a domain

The reference deployment publishes `api` through a Cloudflare Tunnel with edge rate
limiting in front of the anonymous page. Any reverse proxy works; keep `API_CLIENT_IP=peer`
unless Cloudflare is the only route to the origin, for the reason the
[security page](../../reference/security/) gives.
