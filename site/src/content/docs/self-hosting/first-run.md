---
title: Requirements and first run
description: What your own judgebot needs, in what order to bring it up, and what each step costs.
sidebar:
  order: 1
---

Each community runs its own judgebot, with its own Discord application, model key and
spend cap. A complete instance is three containers from one compose file:

- `db`: Postgres with pgvector.
- `judgebot`: the Discord bot, the web app and HTTP API, and the daily data refresh.
- `proxy`: Caddy in front of `judgebot`, on `127.0.0.1:8787`.

What `judgebot` runs is a set of roles, `JUDGE_ROLES` in `.env`: `--discord`, `--api`,
`--web`, `--mcp`, `--admin` (the operator's pages) and `--jobs` (the refresh). Unset, it
runs every role but `--mcp` and `--admin`, which each need a token of their own. A
further service, `refresh`, runs the data loads on demand. Two optional ones put the
instance on the internet: `edge` serves it over HTTPS on ports 80 and 443, and
`cloudflared` through a Cloudflare Tunnel without opening a port. You can stop at the
web app and never touch Discord. The bot is the last thing to add.

## Requirements

- A host that stays on, with Docker and the compose plugin. Running takes about 200 MB of
  RAM across the two containers. `scripts/deploy.sh` pulls the CI-built image.
  **Building** it from the checkout (`scripts/deploy.sh --build`) takes ~4 GB and a lot
  of CPU.
- No Rust. The image carries `judgebot ingest` as its `refresh` service, so the data loads
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
   4. Set `JUDGE_OPERATOR_EMAIL` (a support address, required by the web app and the
      API), and `JUDGE_OPERATOR_DISCORD` (your Discord username, required by the bot) if
      you know it already.
   5. Set `JUDGE_ROLES` to `--api --web --jobs` in the Roles group: the web app and the
      refresh, without Discord until you add the bot.
   6. Check the side panel, which shows whether each role would start. Save on the Review
      tab, then Ctrl-C.

   To edit by hand instead, `cp .env.example .env` and fill it in. Every variable is
   documented in the file and in the
   [configuration reference](../../self-hosting/configuration/).
2. `scripts/deploy.sh --db` starts the database alone. Postgres publishes on
   **localhost:5432** (loopback only). If something on the host already has that port,
   set `DB_PORT` and change the port in `DATABASE_URL` to match.
3. `docker compose run --rm refresh init` loads everything: the schema, the cards, the
   current rules, the alias and note lists, embeddings if you have an embedder, and the
   emoji if `DISCORD_TOKEN` is already set. `init` is safe to run again. It counts as a
   refresh, so the first scheduled one comes a day later.
   The first embed pays the embedder once for every rule and glossary entry, a few cents
   on Voyage. After that, only changed rules are re-embedded.
4. `scripts/deploy.sh` starts `judgebot` and `proxy`, and waits until `judgebot` is
   healthy. It applies pending migrations at startup unless `JUDGE_AUTO_MIGRATE=false`.
   Open <http://localhost:8787> and ask a question. This runs the full pipeline, so it
   checks the model setup before Discord is involved.
5. [Create the Discord app](../../self-hosting/discord-app/), clear `JUDGE_ROLES` (or
   add `--discord` to it), then `scripts/deploy.sh`, which puts the edited `.env` live
   without downtime. Then run `docker compose run --rm refresh emoji` once, so answers
   show mana symbols as pictures instead of `{W}`.
6. Turn on the weekly backup to an S3-compatible bucket (Cloudflare R2, AWS S3, Backblaze
   B2, MinIO): add `backup` to `COMPOSE_PROFILES`, fill in `.env.deploy`, then
   `scripts/deploy.sh`, which starts the `backup` service. The
   [deployment runbook](../../self-hosting/deployment/) has each provider's settings, the
   steps, the restore drill, and the cron line for `scripts/backup-db.sh` instead. The data
   refresh needs no schedule of yours: `judgebot`'s `--jobs` role runs it every
   `JUDGE_REFRESH_HOURS` (default 24), once step 3 has loaded the data. Until then it
   waits for it, with one warning naming `init`.

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
repository root. Each command replaces its whole table and records the list as yours, so
the daily refresh and a later `init` leave it alone. That also opts it out of updates to
the built-in list until you run the command again with no file. A built-in list is
reloaded by the refresh when an upgrade changes it.

```sh
docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml
```

## Embedding width

The schema is created with `vector(1024)` columns, Voyage's width. Your embedder may
produce another width (OpenAI's `text-embedding-3-small` is 1536):

- `init` handles it. On a database with no vectors yet, it takes the configured
  embedder's width.
- Loading step by step, run `reembed --yes` instead of `embed` the first
  time. It retypes the columns and records the embedding space before filling them.

The bot never mixes vectors from two embedders. On a mismatch it logs an error naming both
and runs without vector search.

## Behind a domain

Until a profile says otherwise, the stack answers on `127.0.0.1:8787` only. That is
enough for Discord over its gateway connection, which dials out. To publish the web app:

1. Point a DNS `A` record for the name at the host, not proxied by Cloudflare, and
   forward ports 80 and 443 to it. Leave out an `AAAA` record unless the compose network
   has IPv6: otherwise every IPv6 visitor reaches `edge` from Docker's gateway and they
   share one rate-limit bucket (the runbook has the override that enables it).
2. Set in `.env`:

   ```ini
   COMPOSE_PROFILES=public
   JUDGE_DOMAIN=judge.example.com
   API_CLIENT_IP=proxy
   ```

3. `scripts/deploy.sh`. It starts the bundled Caddy (`edge`), which gets a Let's Encrypt
   certificate and serves `https://judge.example.com`.

A reverse proxy of your own, or a Cloudflare Tunnel with no open port, works instead.
The [deployment runbook](../deployment/#3-exposure) has all three. `API_CLIENT_IP` must
match the route, or the rate limit counts the wrong address; the
[security page](../../reference/security/) gives the reason.
