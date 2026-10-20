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
| `JUDGE_ROLES` | compose: `--discord --api --web --jobs` | What `judgebot` runs: `--discord` (the bot), `--api` (`POST /api/judge`), `--web` (the web app), `--mcp` (the MCP transport), `--admin` (the [admin interface](#admin-interface)), `--jobs` (the scheduled refresh), at least one, separated by spaces and quoted. The compose service passes it as its command; `judgebot` itself reads it when its command line names no role. `'--api --web --jobs'` runs without Discord. |

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
| `JUDGE_LOCALE` | `en` | every interface | The language replies are in when the asker's own is not one the bot speaks: Discord tries the user's language, the web app and the admin pages the browser's (a server's own language setting is not used: Discord sets it to US English unless an admin changes it, so it would hide this one), `judge-cli`, `judgebot` and `judge-config` the terminal's (`LC_ALL`, `LC_MESSAGES`, `LANG`), and `judge-config`'s page the browser's (judge-config falls back to the value in the `.env` it edits). The alert webhook's messages are always in this language; the backup service reads it from `.env` through Compose. Only English (`en`) ships today. The rules material, the answers, JSON output and diagnostics (logs, the command lines' errors, health checks) stay English; the API's and the bot's own error replies are translated. A value that names no supported language stops every binary that loads the configuration. |
| `JUDGE_CALL_RETENTION_DAYS` | unset (keep every call) | the refresh | Delete answered calls older than this many days, with their ratings: 1 to 3650, or unset / `0` to keep them all. 90 is a sensible choice. Each refresh's `prune` step deletes them, as does `judgebot ingest prune`; with `JUDGE_REFRESH_HOURS=0`, cron one of those. A deleted call is no longer an example for later answers or a thread's history. Each run's usage record (`judge_runs`) is kept. A value out of range stops every binary that loads the configuration. |
| `FEEDBACK_IP_DAYS` | `90` | judgebot, the refresh | How many days a rating given on the web keeps the address and user agent it came from, for dealing with abuse: 1 to 3650. After that both are cleared, and the rating and its reason are kept. Every rating written clears the older ones, and so does each refresh's `sweep` step. There is no "keep forever". Discord ratings store neither. A value out of range stops every binary that loads the configuration. |
| `JUDGE_REFRESH_HOURS` | `24` | `--jobs` | Hours between data refreshes (Scryfall cards and rulings, a new Comprehensive Rules release, embeddings, emoji), which the `--jobs` role runs. 1 to 720, or `0` for off (keep a cron'd `scripts/refresh-data.sh` instead). Every process and a cron run take turns through a lock in the database, so a refresh runs once however many processes there are. A failed refresh is retried after an hour, then less often while it keeps failing. A container asked to stop (a deploy) stops its refresh, recorded as neither a success nor a failure, and the new container runs it again at its first check. It waits for the first load (`init`) on an empty database. A scheduled run never embeds more than 800 rows; above that it skips the step and alerts. A value out of range stops every binary that loads the configuration (`judge-cli`, `judge-mcp`, `judge-eval` and `judgebot ingest` too), as `JUDGE_BUDGET_PERIOD` does. Set `0` in a development `.env`, or `cargo run` with `--jobs` refreshes your local database for real. |
| `JUDGE_SPEND_WARN_AT` | (none) | judgebot, `judgebot ingest` | Warn before the cap is reached, at these percentages of `JUDGE_MAX_USD`: comma-separated whole numbers from 1 to 99, such as `50,80`. Each threshold is logged and posted to `JUDGE_ALERT_WEBHOOK` once per period. A check that finds several passed reports the highest. In a `day` or `month` period one process sends each warning however many share the cap (claimed in the `spend_warnings` table). A cap raised mid-period, or an ingest run under a raised one, warns at its own percentages. Spend counts calls in flight at their worst case (about $0.36 each), so with several questions in flight a threshold can fire early. Questions are refused about $0.36 short of the cap, so a threshold that close may never be reached. Blank: no warnings. A malformed value stops every binary that loads the configuration, as `JUDGE_BUDGET_PERIOD` does. |
| `JUDGE_ALERT_WEBHOOK` | | judgebot, scripts, backup | An `https` webhook (Discord, Slack or compatible). It is called the first time the cap refuses a question in a period (once per process), and when spending reaches a `JUDGE_SPEND_WARN_AT` threshold. A refused embedding does not call it: the question is still answered, without vector search, and a refresh whose embed step stops at the cap reports as a failed refresh. It is also called when a scheduled refresh fails (once per streak of failures, saying when it timed out), succeeds again, first skips embedding at the 800-row ceiling or crashes, when `scripts/refresh-data.sh` or `scripts/backup-db.sh` fails, when `DISCORD_INTERACTIONS` disagrees with the application's Interactions Endpoint URL at startup, and when an interaction over HTTP is refused for its timestamp (once per streak: a host clock five minutes wrong refuses them all). The `backup` service reads its own copy from `.env.deploy` and posts the first failure of a streak, a failure at another step, and the recovery. The URL is a credential. Without it, a tripped cap, a spend warning or a failed refresh is only a line in the log. |
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
| `DISCORD_INTERACTIONS` | `gateway` | How Discord delivers commands and button clicks. `gateway`: over the connection one `judgebot` holds. `http`: POSTed to `/discord/interactions` on `API_ADDR`, which must be reachable at a public HTTPS address saved as the application's Interactions Endpoint URL; no gateway connection is opened, and interactions keep being answered while a deploy switches containers. [Interactions over HTTP](../discord-app/#7-interactions-over-http). |

## HTTP API and web app

| Variable | Default | Meaning |
| --- | --- | --- |
| `API_ADDR` | `0.0.0.0:8787` | Listen address of the network roles (`--api`, `--web`, `--mcp`) and of Discord's interactions endpoint with `DISCORD_INTERACTIONS=http`, which also serves `GET /api/health` and `GET /api/about` whichever of them is on (with `--discord` alone over HTTP, `/api/health` only). Keep `0.0.0.0` in Docker so `cloudflared` and the `proxy` service can reach it. The container publishes no port; the `proxy` service publishes `127.0.0.1:8787`, loopback only. |
| `WEB_DIST` | `web/dist` | The built web app, read only under `--web`. The image sets `/srv/web`. A `--web` launch with no `index.html` there is refused at startup. |
| `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS` | `4`, `300` | Questions per IP per window, checked before the concurrency semaphore and the spend cap. |
| `API_CLIENT_IP` | `peer` | `peer` (socket address) or `cloudflare` (`CF-Connecting-IP`). Never `X-Forwarded-For`. See [Security](../../reference/security/). |
| `JUDGE_FEED` | on | `false` removes the public [feed](../../using/web/#the-feed) and [dashboard](../../using/web/#the-dashboard): the `/feed`, `/calls/<id>`, `/users/<name>` and `/dashboard` pages and `GET /api/feed` / `GET /api/calls/{id}` / `GET /api/stats`. On, every `--web` listener serves it, listing every answered call not asked private, earlier ones included, with the Discord asker's username. `/help` says so unless it is off. A value that is not a boolean stops every binary that loads the configuration. |
| `JUDGE_FEED_DAYS` | unset (no limit) | Hide calls older than this many days from the feed, its pages and the dashboard's counts: 1 to 3650, or unset / `0` for none. 90 is a sensible choice. Nothing is deleted, and older calls still serve as examples; `JUDGE_CALL_RETENTION_DAYS` deletes. A value out of range stops every binary that loads the configuration. |
| `JUDGE_PUBLIC_FEEDBACK` | `false` | Show, under each answer on the feed and call pages, the reasons raters gave with a *Partially correct* or *Incorrect* rating: the kind of problem and their words, never who gave them (no name, user id or address). Raters are told so in the form and the confirmation, on Discord and the web. Unset or `false`: the reasons are the operator's alone (the admin pages, `judge-cli queue`). The counts are public either way. **Turning it on publishes free text from anonymous web raters**: 30 ratings per address per ten minutes, and a fresh session id per request makes each a new rater. Only user, role and channel mentions are reduced (to `@…` and `#…`) and `@everyone` / `@here` broken; nothing else is filtered. **Dismissing** a feedback-queue item takes its reason off the public pages. A value that is not a boolean stops every binary that loads the configuration. |
| `JUDGE_PUBLIC_COST` | `false` | Show, on the public [dashboard](../../using/web/#the-dashboard), what the public questions cost: their runs' model charges in all and per question (`GET /api/stats`'s `cost`). Unset or `false`: spend is the operator's alone (the admin dashboard, `judge-cli stats`). Private questions are never counted either way. A value that is not a boolean stops every binary that loads the configuration. |
| `JUDGE_PUBLIC_MODELS` | `false` | Show, on the public dashboard, which models read and answered the public questions, and let it narrow to one model (`GET /api/stats`'s `models` and `model`). Unset or `false`: a `model` filter there is refused. A value that is not a boolean stops every binary that loads the configuration. |
| `MCP_TOKEN` | | The bearer token for `/mcp` (24+ printable ASCII bytes). The endpoint also needs `--mcp`. `--mcp` without a token fails at startup. A token without `--mcp` serves nothing and logs a warning. |
| `MCP_ALLOWED_HOSTS` | loopback only | Comma-separated `Host` values the MCP transport accepts: the public hostname, plus `localhost` if you curl on the host. |
| `MCP_JUDGE_LIMIT`, `MCP_JUDGE_WINDOW_SECS` | `20`, `3600` | `judge` runs per window through `/mcp`. This limits the damage a leaked token can do. |
| `MCP_ADMIN_TOKEN` | | A second `/mcp` token that also reaches the admin tools (`admin_calls`, `admin_call`, `admin_failures`, `admin_usage`), which read every stored call with its asker. Unset: `/mcp` has no admin tools, and `MCP_TOKEN` never does. 24+ printable ASCII bytes, different from `MCP_TOKEN` and `ADMIN_TOKEN`, or no process that reads it starts. For agents that cannot run a command; one that can runs stdio `judge-mcp` over SSH instead ([Agents](../../using/agents/#admin-tools)). |

## Admin interface

The operator's pages: every stored call, private ones included, the failed calls, and
the usage and spend ([Web app](../../using/web/#admin-pages)). Read only under `--admin`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `ADMIN_ADDR` | `127.0.0.1:8789` | Where `--admin` listens, outside compose. The compose service sets `0.0.0.0:8789` inside the container whatever `.env` says, and the `proxy` service publishes it on the host's loopback only. The tunnel never points at it; from another machine, forward the port over SSH. It may not share `API_ADDR`'s port. |
| `ADMIN_PORT` | `8789` | The host port compose publishes the admin interface on (`127.0.0.1` only). The proxy publishes it whether `--admin` runs or not, so set a free port when something else has 8789, or the proxy cannot start. `judge-cli admin-url` in the container prints it. |
| `ADMIN_TOKEN` | (required by `--admin`) | The admin pages' bearer token: 24+ printable ASCII bytes (`openssl rand -base64 32`), different from `MCP_TOKEN` and `MCP_ADMIN_TOKEN`. `judge-cli admin-url` prints the page's address with it in the `#token=` fragment. |

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
