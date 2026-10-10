---
title: Command reference
description: The judgebot roles, every subcommand of judgebot ingest and judgebot backup, judge-eval and judge-cli, and the compose and script entry points.
sidebar:
  order: 5
---

Each binary prints its usage with `--help`. Every binary reads `.env` from the working
directory, and `judgebot backup` reads `.env.deploy` before it. A missing file is fine,
and a malformed one is an error. The containers get theirs through compose.

## `judgebot`

The one long-running binary. What a process does is a set of roles, at least one, named
as flags: `cargo run --release -p judgebot -- --api --web`. With no flags it reads
`JUDGE_ROLES`, the same flags separated by spaces. Every role's requirements are checked
before anything connects or binds, and every unmet one is reported at once. The serving
roles also need a chat model that builds (`ANTHROPIC_API_KEY` or a `judge.toml`). The
HTTP listener is bound before the Discord gateway is contacted. Of the processes running
`--discord` on one database, only the one holding the gateway lease connects; the others
stand by and take over when it goes.

| Role | What it runs | Requires |
| --- | --- | --- |
| `--discord` | The Discord bot | `DISCORD_TOKEN`, `JUDGE_OPERATOR_DISCORD` |
| `--api` | `POST /api/judge`, the anonymous question route | `JUDGE_OPERATOR_EMAIL` |
| `--web` | The built web page, from `WEB_DIST` | `JUDGE_OPERATOR_EMAIL`, a built `index.html` |
| `--mcp` | The MCP transport at `/mcp` | `JUDGE_OPERATOR_EMAIL`, `MCP_TOKEN` |
| `--jobs` | The scheduled data refresh, every `JUDGE_REFRESH_HOURS` (`0` turns it off) | a schedule that is on, when it is the only role |

The network roles (`--api`, `--web`, `--mcp`) share one listener on `API_ADDR`, which
also serves `GET /api/health` and `GET /api/about`. The roles of one process share one
spend meter, so `JUDGE_MAX_USD` caps them together. `--jobs` against a development
database refreshes it for real: leave it off there, or set `JUDGE_REFRESH_HOURS=0`.

When two serving roles run together, the first to stop ends the process with a non-zero
status, so the restart policy brings it back whole. The refresh runs on a thread of its
own. If that thread ends beside serving roles, it is logged as an error and the process
keeps answering. In a process whose only role is `--jobs`, the process exits non-zero.

The compose service `judgebot` runs `JUDGE_ROLES` when it is set in `.env`, else
`--discord --jobs` plus the deprecated `API_INTERFACES` (default `--api --web`). With
neither variable set, that is every role but `--mcp`. A process started with
`API_INTERFACES` set logs a warning naming the `JUDGE_ROLES` line that replaces it.

The image also answers to the names of the binaries `judgebot` replaced, as links to it.
Each logs a warning naming its replacement, and a later release removes them. They ignore
`JUDGE_ROLES`.

| Name | Runs |
| --- | --- |
| `judge-bot` | `judgebot --discord --jobs` |
| `judge-api [--api] [--web] [--mcp]` | Those roles and `--jobs`. With no flags, `--api` alone. |
| `judge-ingest <command>` | `judgebot ingest <command>` |

## `judgebot ingest`

Run `cargo run --release -p judgebot -- ingest <command>`. In the image, run
`docker compose run --rm refresh <command>` for the commands that need no file from the
repository. The image has the binaries and the cache volume, not `data/`. Mount a file
with `-v ./data:/data:ro` for `aliases`, `notes` and `rules <path>`.

Every command that writes data takes the refresh lease first, a database lock, and waits
up to an hour for a command that holds it, so two never overlap. `migrate` (its own lock) and `emoji`
(no database) do not.

| Command | What it does |
| --- | --- |
| `init` | The whole first load: `migrate`, `cards`, `rules latest`, `aliases`, `notes`, `retire`, `embed`, `emoji`, each logged with its time. It is recorded as a manual refresh run, so the schedule counts the load as a refresh. Stops at the first failure. Every step is idempotent, so run it again. `embed` skips itself with no embedder and `emoji` with no `DISCORD_TOKEN`. On a database holding no vectors it takes the configured embedder's width, as `reembed --yes` would. It loads the built-in alias and note lists, recorded as built-in, except a list you loaded from a file, which it keeps. |
| `migrate` | Apply pending schema migrations. `judgebot` does this at startup unless `JUDGE_AUTO_MIGRATE=false`. |
| `cards` | Scryfall bulk sync: cards, faces, printed names, rulings. Cached in `INGEST_CACHE_DIR`. |
| `rules <url\|path>` | Parse a Comprehensive Rules text file into rule-level and leaf rows. Rules whose text changed lose their embedding. |
| `rules latest` | The CR linked from Wizards' rules page, only if its date differs from the stored `cr_version`. |
| `aliases [yaml]` | Load the nickname → card list: the copy of `data/aliases.yaml` built into the binary, or the file named. Replaces the table, and records which: the refresh keeps a built-in list current and leaves a file alone. |
| `notes [yaml]` | Load the hand-written notes for "nightmare" cards: the built-in copy of `data/notes.yaml`, or the file named. Replaces the table and records which, as `aliases` does. |
| `embed` | Embed rows with no vector, with the configured embedder. Refuses if the database holds another embedding space or the columns' width differs. |
| `reembed [--yes] [--clear]` | Make the database hold the configured embedder's space: retype the columns, clear every vector, record the space, then embed all. Without `--yes` it prints the row counts and a rough cost and changes nothing. When the database is already in the right space, it only fills empty rows. `--clear` re-pays every row. |
| `emoji` | Upload Scryfall's card symbols as the bot's application emoji. Needs only `DISCORD_TOKEN`. |
| `retire` | Retire calls whose citations no longer hold against current rules, rulings and Oracle text. Restore those that hold again. |
| `refresh` | `cards`, `rules latest`, `lists` (reload a built-in alias or note list an upgrade changed), `retire`, `embed`, `emoji`. Every step runs even if one fails, and the exit code is ≠ 0 if any did. `embed` with no embedder and `emoji` with no `DISCORD_TOKEN` are skipped, not failed. Each run is recorded in `refresh_runs`. `judgebot --jobs` runs the same steps every `JUDGE_REFRESH_HOURS`, taking turns with this command through the lease. |

## `judgebot backup`

The database backup to Cloudflare R2 (or any S3-compatible store). In the image, run
`docker compose run --rm backup <command>`. The settings are `.env.deploy`'s (`R2_*`,
`BACKUP_*`, `JUDGE_ALERT_WEBHOOK`), plus `DATABASE_URL` for `run` and `serve`; the
[configuration reference](../../self-hosting/configuration/) lists them. A missing or
malformed one is an error naming each. Logs go to standard error.

| Command | What it does |
| --- | --- |
| `run` | Dump the database (`pg_dump -Fc`, gzipped), refuse a dump under `BACKUP_MIN_BYTES`, upload it as `judgebot-<UTC stamp>.dump.gz`, then prune backups older than `BACKUP_KEEP_DAYS` (never the two newest) and copy it to `BACKUP_KEEP_LOCAL` when set. Exits non-zero and posts to `JUDGE_ALERT_WEBHOOK` on a failure. |
| `list` | The objects under `BACKUP_PREFIX`, oldest first. |
| `fetch <name> [file]` | Download one to `file`, or to standard output, which must not be a terminal (`docker compose run --rm --no-deps -T backup fetch <name> > <name>`). |
| `serve` | The `backup` compose service: a backup whenever the newest in the bucket is `BACKUP_EVERY_DAYS` old, checked hourly. Retries a failure after an hour, then less often, and posts the first failure of a streak, a failure at a different step, and the recovery. |

`scripts/backup-db.sh` writes the same objects, so each reads the other's backups.

## `judge-eval`

`cargo run -p judge-eval -- <command>`.

| Command | What it does |
| --- | --- |
| `recall [--vectors]` | The retrieval gate: are the gold set's expected rules in the retrieved context? Free. `--vectors` adds the vector search (~$0.001). Exit ≠ 0 below 90% retrieved or 75% shown within the synthesis budget. |
| `answer --label L [--limit N] [--ids a,b] [--max-usd X] [--out path] [--gold path] [--gold-extraction] [--grade] [--config judge.toml]` | A live run of the full pipeline over the gold set, scored and stored under `eval/runs/`. About $1.70 for all 22 questions. `--grade` grades the answers afterwards, as `grade` does, on the same spend cap. |
| `grade <run.json> [--max-usd X] [--config judge.toml] [--gold path] [--force]` | The configured synthesis model grades each stored answer: whether its ruling follows from its quotes, whether it agrees with the reference, and which remarks are wrong. Written into the run file. About $0.02 an answer, capped at `--max-usd` (default $1.00). Run it again to resume. `--force` grades every answer again. Exit ≠ 0 when a grade failed or the pass stopped. |
| `rescore <run.json>` | Re-score a stored run after a gold-set edit. Free. |
| `show <run.json>` | Bot and gold answers side by side, with each answer's grade. |

## `judge-cli`

`cargo build --release -p judge-agent`, then `target/release/judge-cli <command>`.
JSON goes to stdout and logs to stderr.

| Command | What it does |
| --- | --- |
| `judge <question> [--thread T] [--pin span=Name]...` | The built-in pipeline. Spends model budget. |
| `begin <question> [--thread T]` | Open a session. Returns the extraction prompt. |
| `prompt <session>` / `status <session>` | The current prompt / the session's stage. |
| `extract <session> <file\|->` | Submit the extraction. Returns the synthesis prompt. |
| `rules <session> <id>...` | The one `lookup_rules` round. |
| `verdict <session> <file\|-> [--persist]` | Submit the verdict. It is validated, or rejected with a notice. |
| `persist <session>` | Persist an admitted verdict (idempotent). |
| `card <name>` / `card-info <uuid>` | Resolve a name / a card by id. |
| `get-rules <id>...` / `search <query> [--limit N]` / `glossary <term>` | Rules text, full-text search, glossary. |
| `failures [--limit N]` | The newest failed calls (default 10, at most 100), newest first: when, thread, whether it was asked privately, the question, the error, why the first attempt was rejected and the answer text of each attempt. Read from `failed_calls`. CLI only. |
| `stats [--days N]` | The operator's view, over the last N UTC days (default 30): stored questions per day by interface, estimated model spend and model calls per day (from the `spend_days` ledger every serving process keeps), ratings by score, retired calls, the ten worst-rated calls, and the last five data refresh runs (started, trigger, process, outcome `ok`/`failed`/`stopped`/`running`/`abandoned`, CR version before and after, failed steps; on a schema without the run table, none and a `refresh_runs_note`). A Discord question asked with `private: True` is in the spend and not in the questions. CLI only. |
| `config` | The resolved provider setup, secrets redacted. |
| `about` | The source offer: the repository holding this instance's source, the commit it was built from, the licence and copyright, and the data's `freshness` (the Comprehensive Rules release loaded and the last refresh). Without a database `freshness` is `null` and the rest still prints. |

## Compose and scripts

| Entry point | What it does |
| --- | --- |
| `docker compose up -d` | `db` and `judgebot`, plus `cloudflared` with `tunnel` in `COMPOSE_PROFILES` and `backup` with `backup`. |
| `docker compose up -d --build judgebot` | Rebuild and redeploy after code changes. |
| `docker compose pull && docker compose up -d --remove-orphans` | Deploy host: pull the CI-built image, never build. `--remove-orphans` removes the `bot` and `api` containers of a compose file from before `judgebot`, which would otherwise keep running. |
| `docker compose logs judgebot` | The process's log: the roles it runs, then each role's lines. |
| `docker compose run --rm --entrypoint judge-cli judgebot <command>` | `judge-cli` from the image. |
| `scripts/refresh-data.sh` | A refresh now, or from your own cron with `JUDGE_REFRESH_HOURS=0`: `docker compose run --rm refresh`. Arguments pass through to `judgebot ingest`. A failed run posts to `JUDGE_ALERT_WEBHOOK` when that is set. |
| `docker compose run --rm backup run\|list` | The `backup` service's commands by hand: a backup now, or the objects in the bucket. `docker compose logs backup` shows the scheduled ones. |
| `docker compose run --rm --no-deps -T backup fetch <name> > <name>` | Download one backup for the restore drill. `-T` keeps the bytes off a terminal. |
| `scripts/backup-db.sh [list\|fetch]` | The same backup from a host cron, without the `backup` profile: `pg_dump` to Cloudflare R2. `list` and `fetch` serve the restore drill. A failed backup posts to `JUDGE_ALERT_WEBHOOK` (from `.env.deploy`, else `.env`). |
| `scripts/alert.sh` | Sourced by `scripts/refresh-data.sh` and `scripts/backup-db.sh`: posts one line to the webhook, passing the URL on stdin so it never shows in `ps`. |
