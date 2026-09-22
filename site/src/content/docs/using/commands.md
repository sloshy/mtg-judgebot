---
title: Command reference
description: Every subcommand of judge-ingest, judge-eval and judge-cli, and the compose and script entry points.
sidebar:
  order: 5
---

Each binary prints its usage with `--help`. Every binary reads `.env` from the working
directory. A missing file is fine, and a malformed one is an error. The containers get
theirs through compose.

## `judge-ingest`

Run `cargo run --release -p judge-ingest -- <command>`. In the image, run
`docker compose run --rm refresh <command>` for the commands that need no file from the
repository. The image has the binaries and the cache volume, not `data/`. Mount a file
with `-v ./data:/data:ro` for `aliases`, `notes` and `rules <path>`.

| Command | What it does |
| --- | --- |
| `init` | The whole first load: `migrate`, `cards`, `rules latest`, `aliases`, `notes`, `embed`, `emoji`, each logged with its time. Stops at the first failure. Every step is idempotent, so run it again. `embed` skips itself with no embedder and `emoji` with no `DISCORD_TOKEN`. On a database holding no vectors it takes the configured embedder's width, as `reembed --yes` would. It loads the built-in alias and note lists, so load your own after it. |
| `migrate` | Apply pending schema migrations. `bot` and `api` do this at startup unless `JUDGE_AUTO_MIGRATE=false`. |
| `cards` | Scryfall bulk sync: cards, faces, printed names, rulings. Cached in `INGEST_CACHE_DIR`. |
| `rules <url\|path>` | Parse a Comprehensive Rules text file into rule-level and leaf rows. Rules whose text changed lose their embedding. |
| `rules latest` | The CR linked from Wizards' rules page, only if its date differs from the stored `cr_version`. |
| `aliases [yaml]` | Load the nickname → card list: the copy of `data/aliases.yaml` built into the binary, or the file named. Replaces the table. |
| `notes [yaml]` | Load the hand-written notes for "nightmare" cards: the built-in copy of `data/notes.yaml`, or the file named. Replaces the table. |
| `embed` | Embed rows with no vector, with the configured embedder. Refuses if the database holds another embedding space or the columns' width differs. |
| `reembed [--yes] [--clear]` | Make the database hold the configured embedder's space: retype the columns, clear every vector, record the space, then embed all. Without `--yes` it prints the row counts and a rough cost and changes nothing. When the database is already in the right space, it only fills empty rows. `--clear` re-pays every row. |
| `emoji` | Upload Scryfall's card symbols as the bot's application emoji. Needs only `DISCORD_TOKEN`. |
| `retire` | Retire calls whose citations no longer hold against current rules, rulings and Oracle text. Restore those that hold again. |
| `refresh` | `cards`, `rules latest`, `retire`, `embed`, `emoji`. Every step runs even if one fails, and the exit code is ≠ 0 if any did. The nightly cron runs this. |

## `judge-eval`

`cargo run -p judge-eval -- <command>`.

| Command | What it does |
| --- | --- |
| `recall [--vectors]` | The retrieval gate: are the gold set's expected rules in the retrieved context? Free. `--vectors` adds the vector leg (~$0.001). Exit ≠ 0 below 90% retrieved or 75% shown within the synthesis budget. |
| `answer --label L [--limit N] [--ids a,b] [--max-usd X] [--out path] [--gold path] [--gold-extraction] [--config judge.toml]` | A live run of the full pipeline over the gold set, scored and stored under `eval/runs/`. About $1.70 for all 21 questions. |
| `rescore <run.json>` | Re-score a stored run after a gold-set edit. Free. |
| `show <run.json>` | Bot and gold answers side by side. |

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
| `stats [--days N]` | The operator's view, over the last N UTC days (default 30): stored questions per day by front door, estimated model spend and model calls per day (from the `spend_days` ledger `bot` and `api` keep), ratings by score, retired calls, and the ten worst-rated calls. A Discord question asked with `private: True` is in the spend and not in the questions. CLI only. |
| `config` | The resolved provider setup, secrets redacted. |
| `about` | The source offer: the repository holding this instance's source, the commit it was built from, the licence and copyright. No database needed. |

## Compose and scripts

| Entry point | What it does |
| --- | --- |
| `docker compose up -d` | `db`, `bot` and `api`, plus `cloudflared` with `COMPOSE_PROFILES=tunnel`. |
| `docker compose up -d --build bot api` | Rebuild and redeploy after code changes. |
| `docker compose pull && docker compose up -d` | Deploy host: pull the CI-built image, never build. |
| `scripts/refresh-data.sh` | Nightly cron: `docker compose run --rm refresh` under a lock. A failed run posts to `JUDGE_ALERT_WEBHOOK` when that is set. |
| `scripts/backup-db.sh [list\|fetch]` | Weekly `pg_dump` to Cloudflare R2. `list` and `fetch` serve the restore drill. A failed backup posts to `JUDGE_ALERT_WEBHOOK` (from `.env.deploy`, else `.env`). |
| `scripts/alert.sh` | Sourced by the two above: posts one line to the webhook, passing the URL on stdin so it never shows in `ps`. |
