---
title: Configuration reference
description: Every environment variable the binaries read, with its default and which process uses it.
sidebar:
  order: 3
---

All configuration is environment variables. `.env.example` is the annotated template.

- Every binary reads `.env` from its working directory. Exporting it into your shell
  (`set -a; source .env; set +a`) is optional.
- The containers get theirs through compose's `env_file`.
- A [`judge.toml`](../models/) chooses providers and models. It names environment variables
  for its secrets and never holds one.

## Database and models

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `DATABASE_URL` | (required) | all | Postgres connection string. Compose overrides it to `db:5432` inside the network. The example points at `localhost:5432`. |
| `DB_PORT` | `5432` | compose only | The loopback port compose publishes Postgres on. Change it together with the port in `DATABASE_URL` when 5432 is taken. |
| `ANTHROPIC_API_KEY` | | bot, api, eval, agent, ingest | The zero-config model setup: Anthropic direct, `claude-opus-5` for both stages. Unused when a `judge.toml` names other providers. |
| `ANTHROPIC_BASE_URL` | Anthropic's | same | Zero-config only. A gateway speaking `/v1/messages`. |
| `VOYAGE_API_KEY` | | bot, api, ingest, eval, agent | Zero-config embeddings (`voyage-3.5`, 1024). Blank turns the vector leg off. |
| `VOYAGE_MODEL`, `VOYAGE_DIMENSIONS` | `voyage-3.5`, `1024` | same | Zero-config embedding model and width. |
| `JUDGE_CONFIG` | | all | Path to a `judge.toml`. Blank: `./judge.toml` if present (for `cargo run`), else the zero-config setup. Compose mounts only the file this names, so under Docker a `./judge.toml` is ignored until this names it. |
| `<provider>_KEY` … | | all | Whatever `api_key_env` names in `judge.toml`, one per provider a stage uses. A table no stage names is parsed but its key is never read. |
| `AWS_*`, `GOOGLE_APPLICATION_CREDENTIALS` | | bot, api | Credentials for the `claude-platform-on-aws`, `bedrock` and `vertex` doors, read through each platform's own credential chain. Checked once at startup. |
| `JUDGE_MAX_USD` | `5` | bot, api, eval, agent | Spend cap in USD, across every provider. Each call reserves its worst-case cost before it is sent. A question is refused once the money left is less than that, so values under about $0.45 refuse synthesis outright. `JUDGE_BUDGET_PERIOD` says what the cap covers. |
| `JUDGE_BUDGET_PERIOD` | `process` | bot, api | What `JUDGE_MAX_USD` caps. `process`: each of `bot` and `api` separately, for its lifetime, in memory. A restart counts from zero, and a compose deployment can spend twice the cap. `day` or `month`: the current UTC day or month, shared by `bot` and `api` through the `spend_days` table and kept across restarts. The processes sync every 10 seconds, so together they can overshoot by what they spend in that time. `judge-cli` and `judge-eval` always cap per process. |
| `JUDGE_ALERT_WEBHOOK` | | bot, api, scripts | An `https` webhook (Discord, Slack or compatible). It is called the first time the cap refuses a question in a period (once per process, so `bot` and `api` each report), and when `scripts/refresh-data.sh` or `scripts/backup-db.sh` fails. The URL is a credential. Without it, a tripped cap is only a `WARN` in the log. |
| `JUDGE_USER_LIMIT`, `JUDGE_USER_WINDOW_SECS` | `6`, `600` | bot | `/judge` questions per Discord member per window. `0` turns the limit off. A "busy" reply does not count. Held in memory. `/card` and `/rule` are not limited. |
| `JUDGE_CONCURRENCY` | `2` | bot, api | Judge runs in flight at once. Further ones get a "busy" reply. The MCP transport shares the API's slots. |
| `JUDGE_AUTO_MIGRATE` | `true` | bot, api | Apply pending schema migrations at startup. Set `false` to manage the schema yourself with `judge-ingest migrate` or sqlx-cli. |
| `JUDGE_SOURCE_URL` | the upstream repository | all | Where your instance's source code is. Every remote interface shows it with the commit the binary was built from and the AGPL-3.0-or-later notice: the web footer and `GET /api/about`, Discord `/help` and `/license`, the MCP instructions and `about` tool, `judge-cli about`. Set it to your fork if you run a modified version. It must be an http(s) URL, or startup fails. |
| `JUDGE_OPERATOR_DISCORD` | (required by `bot`) | all | The Discord username of whoever runs the instance, shown by `/help` and `/license`, and by the other interfaces when set. A username, not a display name or a `name#1234` tag. A leading `@` is dropped. Every binary refuses to start on a malformed value. |
| `JUDGE_OPERATOR_EMAIL` | (required by `api`) | all | A support address, shown by `GET /api/about`, the page footer, and the MCP instructions and `about` tool. Discord shows it too when set. `judge-api` does not start without it, whichever doors it opens. Only letters, digits and `._+-` before the `@`. `judge-cli` and `judge-mcp` on stdio need neither contact, and show them when set. |

## Discord

| Variable | Default | Meaning |
| --- | --- | --- |
| `DISCORD_TOKEN` | (required by `bot`) | The bot token. Also read by `judge-ingest emoji`, which uploads symbols to the same application. |
| `GUILD_ID` | | Register commands in this one guild (instant). Unset: globally (up to an hour). |
| `JUDGE_ROLE` | `Judge` | Members holding a role with this exact name rate as judges: their rating overrides the crowd's. |

## HTTP API and web page

| Variable | Default | Meaning |
| --- | --- | --- |
| `API_INTERFACES` | `--api --web` | The flags the `api` container passes. Read by `docker-compose.yml`, not by the binary. Each front door is opt-in: `--api` (`POST /api/judge`), `--web` (the page), `--mcp` (the MCP transport). `judge-api` run by hand takes them as arguments. With none, it serves the JSON API alone. `GET /api/health` and `GET /api/about` are always served. |
| `API_ADDR` | `0.0.0.0:8787` | Listen address. Keep `0.0.0.0` in Docker so `cloudflared` can reach it. Access is restricted by the published port (loopback only). |
| `WEB_DIST` | `web/dist` | The built web page, read only under `--web`. The image sets `/srv/web`. A `--web` launch with no `index.html` there is refused at startup. |
| `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS` | `4`, `300` | Questions per IP per window, checked before the concurrency semaphore and the spend cap. |
| `API_CLIENT_IP` | `peer` | `peer` (socket address) or `cloudflare` (`CF-Connecting-IP`). Never `X-Forwarded-For`. See [Security](../../reference/security/). |
| `MCP_TOKEN` | | The bearer token for `/mcp` (24+ printable ASCII bytes). The endpoint also needs `--mcp`. `--mcp` without a token fails at startup. A token without `--mcp` serves nothing and logs a warning. |
| `MCP_ALLOWED_HOSTS` | loopback only | Comma-separated `Host` values the MCP transport accepts: the public hostname, plus `localhost` if you curl on the host. |
| `MCP_JUDGE_LIMIT`, `MCP_JUDGE_WINDOW_SECS` | `20`, `3600` | `judge` runs per window through `/mcp`. This limits the damage a leaked token can do. |

## Ingest and deployment

| Variable | Default | Meaning |
| --- | --- | --- |
| `INGEST_CACHE_DIR` | `.cache` (image: `/var/cache/judgebot`) | Where Scryfall bulk files and the CR text are cached. |
| `RUST_LOG` | `info` in compose | Tracing filter. The containers use `info,sqlx=warn` (`serenity=warn` for the bot). |
| `JUDGE_IMAGE`, `JUDGE_IMAGE_TAG` | upstream package, `latest` | Which image `bot`/`api`/`refresh` run (amd64 and arm64). A fork sets its own package. A release version (`1`, `1.0`, `1.0.0`) or a `sha-<short>` tag pins or rolls back. |
| `COMPOSE_PROFILES` | | Set `tunnel` on a deploy host so `up -d` also starts `cloudflared`. |
| `TUNNEL_TOKEN`, `R2_*` | | In `.env.deploy`, read only by `cloudflared` and the backup script, never by the internet-facing containers. |
