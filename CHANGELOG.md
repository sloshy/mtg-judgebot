# Changelog

Notable changes an operator or user would notice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): a major version may change
configuration, the HTTP and MCP interfaces or the schema in a way that needs an
operator's attention, and its entry says what to do. Migrations apply automatically
unless `JUDGE_AUTO_MIGRATE=false`.

**Your data carries forward.** Stored calls, ratings, sessions, the spend ledger and the
loaded cards and rules survive every upgrade, major versions included: schema changes ship
as forward migrations, and a migration that has been released is never edited. Embeddings
do too, unless a release's notes ask for a `reembed`, which pays the embedder again and
never touches calls. A release that needs more than `docker compose pull && docker compose up -d`
(a re-embed, a new required variable) says so under its own heading. Downgrading across a
migration is not supported; restore the backup taken before the upgrade instead.

## [Unreleased]

## [1.0.0] - 2026-09-20

The first release. `docs/ARCHITECTURE.md` describes everything below as it stands, and
`docs/DECISIONS.md` records why.

### Added
- **The judge pipeline.** A Magic: The Gathering rules question goes through extraction
  and classification, card resolution, retrieval and synthesis. Every answer cites the
  Comprehensive Rules, Scryfall rulings, Oracle text or a rated prior call, and every
  citation's quote is checked verbatim against its source before the answer is shown or
  stored, and every rule number the answer's text names must be one of those citations.
  A failed check gets one retry that is told what was rejected.
- **Card resolution that never guesses.** Aliases, printed names, short names and fuzzy
  matches resolve in a fixed order, `[[bracketed]]` names are taken exactly, and an
  ambiguous name becomes a "did you mean?" choice.
- **Retrieval over three legs**: the question's rules category, full-text search and
  vector search (optional, on when an embedder is configured), plus rulings, glossary
  entries, notes on notoriously difficult cards and rated prior calls.
- **The Discord bot.** `/judge` (guild-only) with rating buttons and "did you mean?"
  buttons, `/card` and `/rule` lookups that call no model, `/help`, `/license`, and
  `/forget`, which deletes the caller's ratings, the only per-user data kept.
  `/judge private: True` answers the asker alone and stores nothing. Each member gets
  `JUDGE_USER_LIMIT` questions per window (default 6 per 10 minutes). An *Incorrect*
  rating says where to report a wrong ruling. Ratings shape which prior calls are retrieved, with a judge
  role whose rating overrides the crowd's. Mana and card symbols render as application
  emoji.
- **`judge-api`**, with one flag per front door: `--api` (`POST /api/judge`), `--web`
  (the SolidJS page) and `--mcp` (the MCP transport at `/mcp`, which also needs
  `MCP_TOKEN`). `GET /api/health` and `GET /api/about` are served whatever is switched
  off. The anonymous API is rate limited per client address (`API_CLIENT_IP` is `peer`
  or `cloudflare`), ahead of a concurrency limit and the spend cap.
  The page keeps a session's history and offers "did you mean?" choices. `/mcp` accepts
  only the hosts in `MCP_ALLOWED_HOSTS` and limits `judge` runs per window
  (`MCP_JUDGE_LIMIT`, `MCP_JUDGE_WINDOW_SECS`), which bounds what a leaked token can spend.
- **Agent sessions.** `judge-cli` and the `judge-mcp` server let an outside agent do the
  model's work step by step, under the same citation validation and limits as the
  built-in pipeline, alongside card, rule, ruling and glossary lookups.
- **A spend cap on every model call** (`JUDGE_MAX_USD`), which reserves the worst-case
  cost before a request is sent. `JUDGE_BUDGET_PERIOD=day|month` makes it one budget for
  the period, shared by `bot` and `api` and kept across restarts.
  `JUDGE_ALERT_WEBHOOK` is told when the cap trips and when the nightly refresh or the
  backup fails. `judge-cli stats` shows questions, spend and ratings per day.
- **Providers as configuration.** With only `.env`, the judge runs on Anthropic's API
  with Voyage embeddings if keyed. A `judge.toml` chooses a provider and model per
  stage: Anthropic direct, through a proxy, Claude Platform on AWS, Bedrock or Vertex,
  any OpenAI-compatible chat completions endpoint, and Voyage or OpenAI-compatible
  embeddings. `judge.example.toml` documents every knob.
- **Embedding-space tracking.** The database records which embedder's vectors it holds.
  A mismatch turns the vector legs off instead of mixing spaces, and
  `judge-ingest reembed` switches space.
- **A one-command first load.** `docker compose run --rm refresh init` creates the schema
  and loads the cards, the rules, the alias and note lists (built into the binary) and
  the embeddings, with no Rust toolchain on the host.
- **Data ingest and nightly refresh** (`judge-ingest`): Scryfall cards and rulings, the
  Comprehensive Rules (a new release is detected from Wizards' rules page), aliases,
  notes, embeddings and Discord emoji. A renumbered rule keeps the calls that cite it,
  and a call is retired when its citations or its cards' Oracle text stop holding, and
  restored when they hold again.
- **The source offer and operator contact on every remote interface** (AGPL-3.0-or-later
  §13): the repository, the commit the binary was built from, the licence and who runs
  the instance. `JUDGE_SOURCE_URL` points the offer at a fork. The bot requires
  `JUDGE_OPERATOR_DISCORD` and `judge-api` requires `JUDGE_OPERATOR_EMAIL`.
- **Deployment.** A `docker compose` setup (Postgres with pgvector, `bot`, `api`, an
  optional Cloudflare Tunnel or any reverse proxy, a `refresh` job) with a database-backed healthcheck,
  migrations applied at startup and a backup script for Cloudflare R2. The published
  image is a manifest list for `linux/amd64` and `linux/arm64`. A GitHub release
  `vX.Y.Z` tags the image built for that commit as `X.Y.Z`, `X.Y` and `X`, and
  `JUDGE_IMAGE_TAG` accepts those alongside `sha-<short>`.
- **An evaluation harness** (`judge-eval`): a retrieval gate that needs no API key and a
  21-question gold set for live runs. Two graded runs are published under
  `eval/published/` with the results in the README: on the default configuration 17 of 18
  in-scope questions answered, all 17 agreeing with the reference, and the eighteenth
  asked which card was meant.
- **The documentation site**, organised around running your own judgebot, from the
  canonical files in `docs/`.

[Unreleased]: https://github.com/sloshy/mtg-judgebot/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/sloshy/mtg-judgebot/releases/tag/v1.0.0
