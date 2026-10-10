---
title: Configuration reference
description: Every environment variable the binaries read, with its default and which process uses it.
sidebar:
  order: 3
---

All configuration is environment variables, kept in `.env`. The
[config editor](../config-editor/) (`scripts/config.sh`) is the way to set them: it lists
every variable below with its documentation and checks each change with the binaries' own
loaders. Editing `.env` by hand works too, starting from the annotated `.env.example`.

- Every binary reads `.env` from its working directory. Exporting it into your shell
  (`set -a; source .env; set +a`) is optional.
- The containers get theirs through compose's `env_file`.
- A [`judge.toml`](../models/) chooses providers and models. It names environment variables
  for its secrets and never holds one.
- The config editor never shows a secret, and can replace one with a value you type.

In the tables, `judgebot` is the long-running process whatever its roles, and a role flag
(`--discord`) means a variable only that role reads.

## Roles

| Variable | Default | Meaning |
| --- | --- | --- |
| `JUDGE_ROLES` | compose: `--discord --api --web --jobs` | What `judgebot` runs: `--discord` (the bot), `--api` (`POST /api/judge`), `--web` (the web app), `--mcp` (the MCP transport), `--jobs` (the scheduled refresh), at least one, separated by spaces and quoted. The compose service passes it as its command; `judgebot` itself reads it when its command line names no role. `'--api --web --jobs'` runs without Discord. |
| `API_INTERFACES` | `--api --web` | Deprecated. The interfaces of the `api` service that `judgebot` replaced. While `JUDGE_ROLES` is unset the compose service runs `--discord --jobs` plus these, and logs a warning naming the `JUDGE_ROLES` line to use instead. A later release stops reading it. |

## Database and models

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `DATABASE_URL` | (required) | all | Postgres connection string. Compose overrides it to `db:5432` inside the network. The example points at `localhost:5432`. |
| `DB_PORT` | `5432` | compose only | The loopback port compose publishes Postgres on. Change it together with the port in `DATABASE_URL` when 5432 is taken. |
| `ANTHROPIC_API_KEY` | | judgebot, eval, agent | The zero-config model setup: Anthropic direct, `claude-opus-5-5` for both stages. Unused when a `judge.toml` names other providers. |
| `ANTHROPIC_BASE_URL` | Anthropic's | same | Zero-config only. A gateway speaking `/v1/messages`. |
| `VOYAGE_API_KEY` | | judgebot, eval, agent | Zero-config embeddings (`voyage-3.5`, 1024). Blank turns the vector search off. |
| `VOYAGE_MODEL`, `VOYAGE_DIMENSIONS` | `voyage-3.5`, `1024` | same | Zero-config embedding model and width. |
| `JUDGE_CONFIG` | | all | Path to a `judge.toml`. Blank: `./judge.toml` if present (for `cargo run`), else the zero-config setup. Compose mounts only the file this names, so under Docker a `./judge.toml` is ignored until this names it. |
| `<provider>_KEY` … | | all | Whatever `api_key_env` names in `judge.toml`, one per provider a stage uses. A table no stage names is parsed but its key is never read. |
| `AWS_*`, `GOOGLE_APPLICATION_CREDENTIALS` | | judgebot | Credentials for the `claude-platform-on-aws`, `bedrock` and `vertex` endpoints, read through each platform's own credential chain. Checked once at startup. |
| `JUDGE_MAX_USD` | `5` | judgebot, `judgebot ingest`, eval, agent | Spend cap in USD, across every provider, embeddings included. Each call reserves its worst-case cost before it is sent. A question is refused once the money left is less than that, so values under about $0.36 refuse synthesis outright. `JUDGE_BUDGET_PERIOD` says what the cap covers. |
| `JUDGE_BUDGET_PERIOD` | `process` | judgebot | What `JUDGE_MAX_USD` caps. `process`: each process, all its roles together, for its lifetime, in memory. A restart counts from zero. `day` or `month`: the current UTC day or month, shared by every process through the `spend_days` table and kept across restarts. The processes sync every 10 seconds, so together they can overshoot by what they spend in that time. `judgebot ingest` follows it too, so a re-embed counts toward the period. `judge-cli` and `judge-eval` always cap per process. |
| `JUDGE_REFRESH_HOURS` | `24` | `--jobs` | Hours between data refreshes (Scryfall cards and rulings, a new Comprehensive Rules release, embeddings, emoji), which the `--jobs` role runs. 1 to 720, or `0` for off (keep a cron'd `scripts/refresh-data.sh` instead). Every process and a cron run take turns through a lock in the database, so a refresh runs once however many processes there are. A failed refresh is retried after an hour, then less often while it keeps failing. It waits for the first load (`init`) on an empty database. A scheduled run never embeds more than 800 rows; above that it skips the step and alerts. A value out of range stops every binary that loads the configuration (`judge-cli`, `judge-mcp`, `judge-eval` and `judgebot ingest` too), as `JUDGE_BUDGET_PERIOD` does. Set `0` in a development `.env`, or `cargo run` with `--jobs` refreshes your local database for real. |
| `JUDGE_ALERT_WEBHOOK` | | judgebot, scripts, backup | An `https` webhook (Discord, Slack or compatible). It is called the first time the cap refuses a question in a period (once per process). A refused embedding does not call it: the question is still answered, without vector search, and a refresh whose embed step stops at the cap reports as a failed refresh. It is also called when a scheduled refresh fails (once per streak of failures, saying when it timed out), succeeds again, first skips embedding at the 800-row ceiling or crashes, and when `scripts/refresh-data.sh` or `scripts/backup-db.sh` fails. The `backup` service reads its own copy from `.env.deploy` and posts the first failure of a streak, a failure at another step, and the recovery. The URL is a credential. Without it, a tripped cap or a failed refresh is only a line in the log. |
| `JUDGE_USER_LIMIT`, `JUDGE_USER_WINDOW_SECS` | `6`, `600` | `--discord` | `/judge` questions per Discord member per window. `0` turns the limit off. A "busy" reply does not count. Held in memory. `/card` and `/rule` are not limited. |
| `JUDGE_CONCURRENCY` | `2` | judgebot | Judge runs in flight at once, per interface. Further ones get a "busy" reply. The MCP transport shares the API's slots. Discord has slots of its own, so one process with `--discord` and `--api` runs up to twice this many. |
| `JUDGE_AUTO_MIGRATE` | `true` | judgebot | Apply pending schema migrations at startup. Set `false` to manage the schema yourself with `judgebot ingest migrate` or sqlx-cli. |
| `JUDGE_SOURCE_URL` | the upstream repository | all | Where your instance's source code is. Every remote interface shows it with the commit the binary was built from and the AGPL-3.0-or-later notice: the web footer and `GET /api/about`, Discord `/help` and `/license`, the MCP instructions and `about` tool, `judge-cli about`. Set it to your fork if you run a modified version. It must be an http(s) URL, or startup fails. |
| `JUDGE_OPERATOR_DISCORD` | (required by `--discord`) | all | The Discord username of whoever runs the instance, shown by `/help` and `/license`, and by the other interfaces when set. A username, not a display name or a `name#1234` tag. A leading `@` is dropped. Every binary refuses to start on a malformed value. |
| `JUDGE_OPERATOR_EMAIL` | (required by `--api`, `--web`, `--mcp`) | all | A support address, shown by `GET /api/about`, the web app's footer, and the MCP instructions and `about` tool. Discord shows it too when set. The web app, the API and the MCP transport (`--web`, `--api`, `--mcp`) do not start without it. Only letters, digits and `._+-` before the `@`. `judge-cli` and `judge-mcp` on stdio need neither contact, and show them when set. |

## Discord

| Variable | Default | Meaning |
| --- | --- | --- |
| `DISCORD_TOKEN` | (required by `--discord`) | The bot token. Also read by `judgebot ingest emoji`, which uploads symbols to the same application. |
| `GUILD_ID` | | Register commands in this one server only (instant). Other servers the bot is in, and DMs with it, get none. Unset: globally, in every server and in DMs for all but `/judge` (up to an hour). Switching leaves the other set registered until you [clear it](../discord-app/#4-command-registration). |
| `JUDGE_ROLE` | `Judge` | Members holding a role with this exact name rate as judges: their rating overrides the crowd's. |

## HTTP API and web app

| Variable | Default | Meaning |
| --- | --- | --- |
| `API_ADDR` | `0.0.0.0:8787` | Listen address of the network roles (`--api`, `--web`, `--mcp`), which also serves `GET /api/health` and `GET /api/about` whichever of them is on. Keep `0.0.0.0` in Docker so `cloudflared` can reach it. Access is restricted by the published port (loopback only). |
| `WEB_DIST` | `web/dist` | The built web app, read only under `--web`. The image sets `/srv/web`. A `--web` launch with no `index.html` there is refused at startup. |
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
| `JUDGE_IMAGE`, `JUDGE_IMAGE_TAG` | upstream package, `latest` | Which image `judgebot`, `refresh` and `backup` run (amd64 and arm64). A fork sets its own package. A release version (`1`, `1.2`, `1.2.0`) or a `sha-<short>` tag pins or rolls back. |
| `COMPOSE_PROFILES` | | Set `tunnel` on a deploy host so `up -d` also starts `cloudflared`, and `backup` for the weekly database backup (`tunnel,backup` for both). |
| `TUNNEL_TOKEN`, `R2_*` | | In `.env.deploy`, read only by `cloudflared`, the `backup` service and the backup script, never by the internet-facing containers. |

## Backup

These live in `.env.deploy`, beside `R2_*`. The `backup` service and `scripts/backup-db.sh` read the same ones, with the same defaults, and blank means the default. The [deployment runbook](../../self-hosting/deployment/) has the setup and the restore drill.

| Variable | Default | Meaning |
| --- | --- | --- |
| `R2_ENDPOINT`, `R2_BUCKET` | | `https://<account-id>.r2.cloudflarestorage.com` and the bucket. Required. |
| `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY` | | An R2 API token scoped to Object Read & Write on that bucket. Required. |
| `BACKUP_PREFIX` | `db` | The key prefix the backups live under. |
| `BACKUP_KEEP_DAYS` | `60` | Backups older than this are pruned after a successful upload. The service never prunes the two newest. |
| `BACKUP_MIN_BYTES` | `20000000` | A smaller dump is refused, never uploaded. |
| `BACKUP_KEEP_LOCAL` | | A directory to copy each dump into. For the service, a path inside its container (mount one there). |
| `BACKUP_EVERY_DAYS` | `7` | The service takes a backup whenever the newest in the bucket is this many days old (1 to 365). The script's schedule is cron. |
| `DATABASE_URL` | set by compose | The service reaches `db:5432` over the compose network; compose sets it, as for `judgebot`. |
