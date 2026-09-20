---
title: Configuration reference
description: Every environment variable the binaries read, with its default and which process uses it.
sidebar:
  order: 3
---

All configuration is environment variables. Every binary reads `.env` from its working
directory, and the containers get theirs through compose's `env_file`. Exporting the file
into your shell (`set -a; source .env; set +a`) is optional. `.env.example` is the
annotated template. A `judge.toml` chooses providers and models. It names environment
variables for its secrets and never holds one.

## Database and models

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `DATABASE_URL` | (required) | all | Postgres connection string. Compose overrides it to `db:5432` inside the network. The example points at `localhost:5432`. |
| `DB_PORT` | `5432` | compose only | The loopback port compose publishes Postgres on. Change it together with the port in `DATABASE_URL` when 5432 is taken. |
| `ANTHROPIC_API_KEY` | | bot, api, eval, agent, ingest | The zero-config model setup: Anthropic direct, `claude-opus-5` for both stages. Unused when a `judge.toml` names other providers. |
| `ANTHROPIC_BASE_URL` | Anthropic's | same | Zero-config only. A gateway speaking `/v1/messages`. |
| `VOYAGE_API_KEY` | | bot, api, ingest, eval, agent | Zero-config embeddings (`voyage-3.5`, 1024). Blank turns the vector leg off. |
| `VOYAGE_MODEL`, `VOYAGE_DIMENSIONS` | `voyage-3.5`, `1024` | same | Zero-config embedding model and width. |
| `JUDGE_CONFIG` | | all | Path to a `judge.toml`. Blank: `./judge.toml` if present for `cargo run`, else the zero-config setup. Under Docker a `./judge.toml` is invisible until this names it, because compose mounts the named file. |
| `<provider>_KEY` … | | all | Whatever `api_key_env` names in `judge.toml`, one per provider a stage uses. A table no stage names is parsed but its key is never read. |
| `AWS_*`, `GOOGLE_APPLICATION_CREDENTIALS` | | bot, api | Credentials for the `claude-platform-on-aws`, `bedrock` and `vertex` doors, through the platforms' own chains. Probed once at startup. |
| `JUDGE_MAX_USD` | `5` | bot, api, eval, agent | Spend cap per process across every provider. The cap reserves each call's worst case first, so values under about $0.45 refuse synthesis. It is a total for the life of the process, held in memory: `bot` and `api` have one each, a restart counts from zero, and once the headroom left is smaller than a call's worst case the process refuses questions until it is restarted. |
| `JUDGE_CONCURRENCY` | `2` | bot, api | Judge runs in flight at once. Further ones get a "busy" reply. The MCP transport shares the API's slots. |
| `JUDGE_AUTO_MIGRATE` | `true` | bot, api | Apply pending schema migrations at startup. `false` to manage the schema with `judge-ingest migrate` or sqlx-cli. |
| `JUDGE_SOURCE_URL` | the upstream repository | all | The repository named by the source offer every remote interface makes (the web footer and `GET /api/about`, Discord `/help` and `/license`, the MCP instructions and `about` tool, `judge-cli about`), shown with the commit the binary was built from and the AGPL-3.0-or-later notice. Set it to your fork if you run a modified version. It must be an http(s) URL, and anything else is refused at startup. |
| `JUDGE_OPERATOR_DISCORD` | (required by `bot`) | all | The Discord username of whoever runs the instance, shown by `/help` and `/license`. A username, not a display name or a `name#1234` tag. A leading `@` is dropped. The other interfaces show it too when it is set. A malformed value is refused at startup by every binary. |
| `JUDGE_OPERATOR_EMAIL` | (required by `api`) | all | A support address, shown by `GET /api/about`, the page footer, the MCP instructions and `about` tool. `judge-api` does not start without it, whichever doors it opens. Letters, digits and `._+-` before the `@`. Discord shows it too when it is set. `judge-cli` and `judge-mcp` on stdio show it when set and need neither contact. |

## Discord

| Variable | Default | Meaning |
| --- | --- | --- |
| `DISCORD_TOKEN` | (required by `bot`) | The bot token. Also read by `judge-ingest emoji`, which uploads symbols to the same application. |
| `GUILD_ID` | | Register commands in this one guild (instant). Unset: globally (up to an hour). |
| `JUDGE_ROLE` | `Judge` | Members holding a role with this exact name rate with an override. |

## HTTP API and web page

| Variable | Default | Meaning |
| --- | --- | --- |
| `API_INTERFACES` | `--api --web` | The flags the `api` container passes. Read by `docker-compose.yml`, not by the binary. Each front door is opt-in: `--api` (`POST /api/judge`), `--web` (the page), `--mcp` (the MCP transport). `judge-api` run by hand takes them as arguments, and with none it serves the JSON API alone. `GET /api/health` and `GET /api/about` are served whatever is off. |
| `API_ADDR` | `0.0.0.0:8787` | Listen address. Stays `0.0.0.0` in Docker so `cloudflared` can reach it. The published port restricts access. |
| `WEB_DIST` | `web/dist` | The built web page, read only under `--web`. The image sets `/srv/web`. A `--web` launch with no `index.html` there is refused at startup. |
| `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS` | `4`, `300` | Questions per IP per window, checked before the concurrency semaphore and the spend cap. |
| `API_CLIENT_IP` | `peer` | `peer` (socket address) or `cloudflare` (`CF-Connecting-IP`). Never `X-Forwarded-For`. See [Security](../../reference/security/). |
| `MCP_TOKEN` | | The bearer token for `/mcp` (24+ printable ASCII bytes). The endpoint needs `--mcp` as well. The flag without a token is refused at startup. A token without the flag serves nothing and warns. |
| `MCP_ALLOWED_HOSTS` | loopback only | Comma-separated `Host` values the MCP transport accepts: the public hostname, plus `localhost` if you curl on the host. |
| `MCP_JUDGE_LIMIT`, `MCP_JUDGE_WINDOW_SECS` | `20`, `3600` | `judge` runs per window through `/mcp`. This bounds the damage of a leaked token. |

## Ingest and deployment

| Variable | Default | Meaning |
| --- | --- | --- |
| `INGEST_CACHE_DIR` | `.cache` (image: `/var/cache/judgebot`) | Where Scryfall bulk files and the CR text are cached. |
| `RUST_LOG` | `info` in compose | Tracing filter. The containers use `info,sqlx=warn` (`serenity=warn` for the bot). |
| `JUDGE_IMAGE`, `JUDGE_IMAGE_TAG` | upstream package, `latest` | Which image `bot`/`api`/`refresh` run (amd64 and arm64). A fork sets its own package. A release version (`1`, `1.0`, `1.0.0`) or a `sha-<short>` tag pins or rolls back. |
| `COMPOSE_PROFILES` | | `tunnel` on a deploy host starts `cloudflared` with `up -d`. |
| `TUNNEL_TOKEN`, `R2_*` | | In `.env.deploy`, read only by `cloudflared` and the backup script, never by the internet-facing containers. |
