---
title: Requirements and first run
description: What your own judgebot needs, in what order to bring it up, and what each step costs.
sidebar:
  order: 1
---

Each community runs its own judgebot, with its own Discord application, model key and
spend cap. A complete instance is three containers from one compose file:

- `db`: Postgres with pgvector.
- `api`: the web page and HTTP API.
- `bot`: Discord.

A fourth, `refresh`, runs nightly from cron to keep cards and rules current. An optional
`cloudflared` publishes the API without opening a port. You can stop after `api` and never
touch Discord. The bot is the last thing to add.

## Requirements

- A host that stays on, with Docker and the compose plugin. Running takes about 200 MB of
  RAM across the three containers. **Building** the image takes ~4 GB and a lot of CPU. A
  low-powered host pulls the CI-built image instead (`docker compose pull`).
- No Rust. The image carries `judge-ingest` as its `refresh` service, so the data loads
  run in a container. To work from source, see [Development setup](../../contributing/development/).
  A binary run on the host checks HTTPS against the system's CA certificates
  (`ca-certificates` on Debian and Ubuntu) and refuses to start without them.
- A model provider. With `.env` alone that is Anthropic's API (`ANTHROPIC_API_KEY`).
  A [`judge.toml`](../../self-hosting/models/) chooses anything else.
- Optionally, an embedder for semantic search: a Voyage AI key, or an embedding model on
  an OpenAI-compatible server. Without one the bot searches by category and full text only,
  and logs a warning at startup.
- For Discord, an application of your own and its bot token.
  [Create the Discord app](../../self-hosting/discord-app/) walks through the developer
  portal with links into Discord's documentation.

## Order of operations

1. Configure with the [config editor](../../self-hosting/config-editor/):
   1. `docker compose pull`
   2. `scripts/config.sh`, then open the URL it prints.
   3. Set your model key (`ANTHROPIC_API_KEY`, or a provider in the Models tab), and
      `VOYAGE_API_KEY` for semantic search if you have one.
   4. Set `JUDGE_OPERATOR_DISCORD` (your Discord username, required by the bot) and
      `JUDGE_OPERATOR_EMAIL` (a support address, required by `judge-api`).
   5. Check the side panel, which shows whether each part would start. Save on the Review
      tab, then Ctrl-C.

   To edit by hand instead, `cp .env.example .env` and fill it in. Every variable is
   documented in the file and in the
   [configuration reference](../../self-hosting/configuration/).
2. `docker compose up -d db`. Postgres publishes on **localhost:5432** (loopback only). If
   something on the host already has that port, set `DB_PORT` and change the port in
   `DATABASE_URL` to match.
3. `docker compose run --rm refresh init` loads everything: the schema, the cards, the
   current rules, the alias and note lists, embeddings if you have an embedder, and the
   emoji if `DISCORD_TOKEN` is already set. `init` is safe to run again.
   The first embed pays the embedder once for every rule and glossary entry, a few cents
   on Voyage. After that, only changed rules are re-embedded.
4. `docker compose up -d api`. Open <http://localhost:8787> and ask a question. This runs
   the full pipeline, so it checks the model setup before Discord is involved.
   `api` and `bot` both apply pending migrations at startup unless
   `JUDGE_AUTO_MIGRATE=false`.
5. [Create the Discord app](../../self-hosting/discord-app/), then `docker compose up -d
   bot`. Then run `docker compose run --rm refresh emoji` once, so answers show mana
   symbols as pictures instead of `{W}`.
6. Schedule `scripts/refresh-data.sh` nightly and `scripts/backup-db.sh` weekly. The
   [deployment runbook](../../self-hosting/deployment/) has the cron lines.

## Single steps

`init` is these, in this order, and each can be run by itself:

```sh
docker compose run --rm refresh migrate
docker compose run --rm refresh cards
docker compose run --rm refresh rules latest
docker compose run --rm refresh aliases        # the list built into the binary
docker compose run --rm refresh notes
docker compose run --rm refresh embed          # with an embedder configured
docker compose run --rm refresh emoji          # once the Discord app exists
```

To load your own `aliases` or `notes` list, mount the file and name it, running from the
repository root. Each command replaces its whole table. `init` does too, with the built-in
lists, so load your own after any `init`.

```sh
docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml
```

## Embedding width

The schema is created with `vector(1024)` columns, Voyage's width. Your embedder may
produce another width (OpenAI's `text-embedding-3-small` is 1536):

- `init` handles it. On a database with no vectors yet, it takes the configured
  embedder's width.
- Loading step by step, run `judge-ingest reembed --yes` instead of `embed` the first
  time. It retypes the columns and records the embedding space before filling them.

The bot never mixes vectors from two embedders. On a mismatch it logs an error naming both
and runs without vector search.

## Behind a domain

The deployment runbook publishes `api` through a Cloudflare Tunnel, with rate limiting
at Cloudflare's edge in front of the public page. Any reverse proxy works. Keep
`API_CLIENT_IP=peer` unless Cloudflare is the only route to the origin. The
[security page](../../reference/security/) gives the reason.
