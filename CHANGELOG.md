# Changelog

Notable changes an operator or user would notice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). A major version may change
configuration, the HTTP and MCP interfaces or the schema in a way that needs an
operator's attention. Its entry says what to do. Migrations apply automatically
unless `JUDGE_AUTO_MIGRATE=false`.

**Your data carries forward.** Stored calls, ratings, sessions, the spend ledger and the
loaded cards and rules survive every upgrade, major versions included. Schema changes ship
as forward migrations, and a released migration is never edited. Embeddings survive too,
unless a release's notes ask for a `reembed`. That pays the embedder again and never
touches calls. A release that needs more than `docker compose pull && docker compose up -d`
(a re-embed, a new required variable) says so under its own heading. Downgrading across a
migration is not supported. Restore the backup taken before the upgrade instead.

## [Unreleased]

### Changed

- **The default model is Claude Opus 5.5** (`claude-opus-5-5`) on both stages, and the
  built-in price table knows its rates ($4 input, $20 output per million tokens). On the
  gold set, with the fixes below, it answered all 18 in-scope questions, all agreeing with
  the reference, for about 30% less per run than Opus 5. The README's results and the
  Sample answers page come from that run (`eval/published/v1-opus-5-5.json`). The price
  table lists current models only, so it no longer knows `claude-opus-5`, and an unknown
  Anthropic model now prices at Opus 5.5's rate. A `judge.toml` that still names
  `claude-opus-5` should give it a `[models.<stage>.pricing]` table ($5 input, $25 output),
  or the cap under-counts it.
- **Claude Sonnet 5.5 is the measured budget option.** The built-in price table knows
  `claude-sonnet-5-5` ($2 input, $10 output, $0.20 cache reads per million tokens), so a
  `judge.toml` naming it needs no `pricing` table and the spend cap no longer prices it as
  Opus 5. On the gold set, with the reworded prompts below, it answered all 18 in-scope
  questions, all agreeing with the reference, for a little over half the default's cost
  per answer
  (`eval/published/v1-sonnet-5-5.json`). The README's results and the Model choice page
  compare it with the default.

- **Server-side refusal fallbacks are sent on the direct API only.** Anthropic documents
  the `fallbacks` beta for the Claude API only, so Claude Platform on AWS no longer sends
  it, as the proxy, Bedrock and Vertex doors already did not. A provider table's new
  `refusal_fallbacks = true | false` overrides the door's default either way.
- **The extraction and synthesis prompts were reworded.** Both were written for earlier
  models. The extraction prompt now lives in `crates/bot/src/prompts/extract_system.md`
  beside the synthesis one. Its card-span rules are one list, it says what makes a
  shortened name clear, and a shortened name is replaced by a full name added for a group
  nickname too, which Sonnet 5.5 did not do ("tower" beside "Urza's Tower"). The
  synthesis prompt keeps every rule, with the capitals gone, the face label named as not
  citable, and the retry instructions left to each rejection's own notice.

### Fixed

- **Citations written one field over.** With structured output the model writes keys in
  the schema's order, which was alphabetical: a citation's `id` before its `kind`, a
  ruling's quote before its key. The schema now asks for `kind`, then the reference, then
  `quote`, as the prompt does. The retry after an unreadable citation now says a value was
  in the wrong field rather than assuming a placeholder. Together with the next fix, and
  one gold run before and after, Sonnet 5 went from 6 answered questions to 13.
- **"Did you mean?" for a shorthand the question had already made clear.** The extractor
  sent "tower" beside "Urza's Tower". It now sends the full name alone when the message
  makes the card clear, and still leaves an unclear one ("Teferi's" with no hint which
  Teferi) for the user to pick.
- **A nickname in brackets asked "did you mean?".** `[[bob]]` offered Dark Confidant as a
  choice instead of answering. A bracketed span that is exactly an alias now resolves to
  that alias's card: the alias table names one card for that spelling, so it is not a
  guess. Looser matches in brackets (`[[bob's]]`, a near spelling) are still offered.
- **A second `lookup_rules` call ended the question.** Asking for the tool again after its
  one call, or sending well-formed JSON whose ids are not rule ids, failed as an upstream
  error with no retry. Both are now a rejection that gets the one retry, with a notice
  saying what the call allows. Two of Sonnet 5's five unanswered gold questions failed
  this way. The rerun of an answer cut off at the token limit can no longer fetch rules a
  second time.
- **A card's type line cited as Oracle text failed twice.** The material prints a face's
  name, mana cost and type line on its `[oracle …]` label line, and only the Oracle text
  under it is citable. A citation quoting the label got the retry notice for a mistyped
  quote, so the model sent the same quote again and the question went unanswered (Sonnet
  5.5, Valki // Tibalt). The notice now names the part of the label that was quoted and
  says to drop the citation, and the Cards heading says the label is not citable.
- **An example cited under the wrong rule failed twice.** A rule's examples are printed
  after all of its sub-rules, so an example of 903.3 sits under the 903.3e line and was
  cited as 903.3e. The retry notice said the quote was mistyped, and the model sent it
  again. The notice now names the rule the text belongs to.

## [1.0.0] - 2026-09-20

The first release. `docs/ARCHITECTURE.md` describes everything below as it stands, and
`docs/DECISIONS.md` records why.

### Added
- **The judge pipeline.** A Magic: The Gathering rules question goes through extraction
  and classification, card resolution, retrieval and synthesis. Every answer cites the
  Comprehensive Rules, Scryfall rulings, Oracle text or a rated prior call. Before the
  answer is shown or stored, every citation's quote is checked verbatim against its
  source, and every rule number in the answer's text must be one of those citations. A
  failed check gets one retry that is told what was rejected.
- **Card resolution that never guesses.** Aliases, printed names, short names and fuzzy
  matches resolve in a fixed order, `[[bracketed]]` names are taken exactly, and an
  ambiguous name becomes a "did you mean?" choice.
- **Retrieval over three legs**: the question's rules category, full-text search and
  vector search (optional, on when an embedder is configured), plus rulings, glossary
  entries, notes on notoriously difficult cards and rated prior calls.
- **The Discord bot.** `/judge` (guild-only) with rating buttons and "did you mean?"
  buttons, `/card` and `/rule` lookups that call no model, `/help`, `/license`, and
  `/forget`, which deletes the caller's ratings (the only per-user data kept).
  `/judge private: True` answers the asker alone and stores nothing. Each member gets
  `JUDGE_USER_LIMIT` questions per window (default 6 per 10 minutes). An *Incorrect*
  rating says where to report a wrong ruling. Ratings shape which prior calls are
  retrieved, and a rating from a member with the judge role overrides the crowd's. Mana and card symbols render as application
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
  notes, embeddings and Discord emoji. A renumbered rule keeps the calls that cite it.
  A call is retired when its citations or its cards' Oracle text stop holding, and
  restored when they hold again.
- **The source offer and operator contact on every remote interface** (AGPL-3.0-or-later
  §13): the repository, the commit the binary was built from, the licence and who runs
  the instance. `JUDGE_SOURCE_URL` points the offer at a fork. The bot requires
  `JUDGE_OPERATOR_DISCORD` and `judge-api` requires `JUDGE_OPERATOR_EMAIL`.
- **Deployment.** A `docker compose` setup (Postgres with pgvector, `bot`, `api`, an
  optional Cloudflare Tunnel or any reverse proxy, a `refresh` job) with a
  database-backed healthcheck, migrations applied at startup and a backup script for Cloudflare R2. The published
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
