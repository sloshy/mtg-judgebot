# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project values

- **Compiler enforcement over test discipline.** This project chose Rust specifically so
  invariants live in types: exhaustive enums, nutype newtypes with validators,
  `NonEmpty`, and typestate (`Verdict<Unvalidated|Validated>`, `Synth<Fresh|ToolRequested|Final>`).
  When adding an invariant, prefer making the bad state unrepresentable; a runtime check
  or test is the fallback, not the default. `docs/LANGUAGE_EVALUATION.md` §1 lists the
  nine invariants (I1–I9) the design is built around.
- **`crates/core` has no I/O dependencies.** That dependency-graph fence stands in for
  effect tracking (the one thing Rust doesn't give us). Never add reqwest/sqlx/tokio-net
  to core.
- **The user is cost-sensitive on API spend.** Every model call goes through the
  spend-capped `judge_llm::Metered` (`JUDGE_MAX_USD`, default $5; the cap *reserves*
  worst-case cost before sending, so caps under ~$0.45 refuse synthesis outright). Develop
  against wiremock, not the live API; a full 21-question gold run costs ~$2.50.
- Commits in this repo are managed by Claude: commit completed, verified steps without
  asking. Never commit `.env`, `.cache/`, or `eval/runs/`.

## Commands

Everything needs env from `.env` (`set -a; source .env; set +a`). Postgres runs in
Docker on **localhost:5433** (a native Postgres owns 5432 — never touch it).

```sh
docker compose up -d                 # db (pgvector/pg16) + bot + api; all restart with Docker
docker compose up -d --build bot api # redeploy bot/api after code changes (one image, two entrypoints)
                                     # COMPOSE_PROFILES=tunnel also starts cloudflared (docs/DEPLOYMENT.md)
docker compose pull && docker compose up -d  # deploy host: pulls the CI-built GHCR image, never builds
scripts/backup-db.sh                 # weekly pg_dump -> Cloudflare R2; cron'd on the server
cargo build --workspace
cargo clippy --workspace --all-targets   # must be warning-free; lints deny unwrap/expect/indexing/panic
cargo test --workspace               # includes #[sqlx::test] suites that spin temp DBs off DATABASE_URL
cargo test -p judge-bot possessive   # run a single test by substring
SQLX_OFFLINE=true cargo build --workspace   # must pass; regenerate .sqlx after SQL changes:
cargo sqlx prepare --workspace -- --all-targets

~/.cargo/bin/sqlx migrate run --source crates/bot/migrations
cargo run --release -p judge-ingest -- cards            # Scryfall bulk sync (cached in .cache/)
cargo run --release -p judge-ingest -- rules <url|path> # CR parse from a given file or URL
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed            # only rows with NULL embedding; Voyage
cargo run --release -p judge-ingest -- emoji            # Scryfall card symbols -> the bot's Discord
                                                        # application emoji; idempotent, no DB needed
cargo run --release -p judge-ingest -- rules latest     # the CR linked from Wizards' rules page, only if
                                                        # its version differs from max(rules.cr_version)
cargo run --release -p judge-ingest -- retire           # retire/restore calls by whether their citations
                                                        # (and their context cards' Oracle text) still hold
cargo run --release -p judge-ingest -- refresh          # cards + rules latest + retire + embed + emoji; every
                                                        # step runs even if one fails, exit≠0 if any did
scripts/refresh-data.sh              # nightly cron on the deploy host: `docker compose run --rm refresh`

cargo run --release -p judge-api                        # HTTP API + web page on API_ADDR (default :8787)
npm --prefix web run build           # build the SolidJS page into web/dist (served by judge-api)
npm --prefix web run dev             # Vite dev server, proxies /api to a local judge-api

cargo build --release -p judge-agent                    # target/release/judge-cli + judge-mcp (build before
                                                        # .mcp.json can start judge-mcp)
judge-cli judge "<q>" [--thread T] [--pin span=Name]    # built-in pipeline: real spend, own cap per process
judge-cli begin "<q>" [--thread T] | prompt <s> | status <s> | extract <s> <file|-> | rules <s> <id>..
judge-cli verdict <s> <file|-> [--persist] | persist <s>   # the agent-driven session, step by step
judge-cli card <name> | card-info <uuid> | get-rules <id>.. | search "<q>" [--limit N] | glossary <term>
judge-cli config                                        # the resolved provider/model setup, secrets redacted
judge-mcp                                               # the MCP server on stdio (.mcp.json starts it)
                                                        # remote: judge-api serves /mcp when MCP_TOKEN is set

cargo run -p judge-eval -- recall                       # retrieval gate, no API keys, exit≠0 below 90%
cargo run -p judge-eval -- answer --label L --limit 21 --max-usd 6.00   # full live gold run (~$2.50)
                                                        # --config judge.toml runs it on other providers
cargo run -p judge-eval -- rescore eval/runs/<run>.json # re-score a stored run, zero API cost
cargo run -p judge-eval -- show eval/runs/<run>.json    # bot vs gold answers side by side
```

## Architecture

Pipeline (see `docs/ARCHITECTURE.md` §3, which is kept current): extraction+classification
(one low-effort LLM call, structured output) → card resolution (typed ladder: alias →
possessive-stripped alias → `[[bracket]]` → exact → printed name → short-name-before-comma →
alias-suffix → trigram fuzzy; **never guesses** — genuine ambiguity becomes
`Resolution::Ambiguous` and a Discord "did you mean?" button row) → retrieval (three legs
unioned in priority order: curated category→CR-subsection map, tsvector BM25, pgvector
cosine; plus rulings for all faces, glossary, nightmare-card notes, rated prior calls) →
synthesis (high-effort, one `lookup_rules` tool round max — enforced by typestate — with
citation validation and one retry) → persist + Discord rating buttons.

Crate graph: `core` (domain ADTs, ports, `judge()`, citation validation — pure) ←
`llm` (the provider seam, `docs/proposals/providers.md`: neutral `ChatRequest`/`ChatResponse`,
the open `Backend` trait providers implement and the sealed `ChatModel` port the pipeline
calls — only `Metered<B>` implements it, so every send is behind the spend cap by type;
`SpendMeter` + `Price::{Free, PerToken}`, the shared HTTP retry loop, the
`Synth` typestate and `classify`) ← `anthropic` (a `Backend`: hand-written wire
types we own, `Endpoint` enum, neutral↔wire conversion; schemars → Anthropic's schema
subset via a transform that must keep `additionalProperties:false` and rewrite
`oneOf→anyOf`, applied at conversion time) ← `embed` (Voyage) ← `bot` (sqlx adapters,
`extract.rs`/`synth.rs` over `judge-llm` only, prompts in `crates/bot/src/prompts/`,
serenity/poise Discord layer with pure `render.rs`) and `ingest` / `eval` / `api` (bins) and
`agent` (lib + `judge-cli` / `judge-mcp` bins; `api` mounts its MCP handler).
`judge_bot::build_deps(pool, Models, embedder)` is the single composition root shared by
the bot, eval, the HTTP API and the agent's `judge` tool; `Models::{single, pair}` take
the meter and bare backends and meter them themselves (private fields: no uncapped model,
no foreign meter), and `Models::from_env` is the zero-config setup (Anthropic direct, one
model for both stages, one `SpendMeter`) and the only place that names the Anthropic
backend. `crates/bot/tests/anthropic_golden.rs` pins the four Anthropic request shapes
byte-for-byte against captured fixtures (`UPDATE_GOLDEN=1` re-captures them after an
intended prompt/schema change; review the diff). `api` (+ the SolidJS page in `web/`) is the
anonymous front door: no ratings, stateless "did you mean?" via `pins` → `pin_card`
rewriting, session history via a client UUID (`web:<uuid>` thread ids), per-IP
fixed-window rate limiting (`API_RATE_LIMIT`/`API_RATE_WINDOW_SECS`) ahead of the
concurrency semaphore and the spend cap.

Key cross-file facts that aren't obvious from any one file:

- **Citations are typed and validated.** `Citation::{Rule, ScryfallRuling, OracleText,
  PriorCall}` each carry a verbatim `quote` checked as a substring of the source in
  `Context`; a failed check (or an empty/citation-less verdict on an answerable source)
  becomes a retry with the rejection rendered into the prompt. Only
  `Verdict<Validated>` can reach `CallStore::persist` or Discord rendering.
- **CR chunking is two-granularity.** `rules` rows exist at rule level (`702.19`, body
  includes all lettered sub-rules + examples; these get embeddings and feed retrieval)
  AND as leaf rows (`702.19b`, `parent_id` set; citation targets). Scoring and
  `lookup_rules` treat leaf↔parent as covering each other.
- **Ratings shape retrieval, nothing else.** `calls_rated` view: Bayesian mean (prior
  2.0, weight 3) with a judge-role override (`effective_score`); prior calls below 1.5
  with ≥5 votes are excluded, retired calls are excluded, and prior calls are always
  rendered *after* CR material as examples.
- **A call is retired when its citations stop holding, not when the CR changes.**
  `retire_unsupported` (`db/retire.rs`, run by `ingest retire` and nightly inside
  `ingest refresh`) re-runs `citation_supported` over every stored call against today's
  rules, rulings and Oracle text and sets `calls.retired_at`/`retired_reason` both ways,
  so restored text brings a call back. Each call also carries `context_ids.card_text`
  (an `oracle_fingerprint` per context card) so an erratum retires calls *about* the card
  even when they cited only the CR. `cr_version` on a call is a record, not a gate.
  Rulings are keyed by content (`ruling_key`, 16 hex chars, in core because the ingest
  writer and every reader must agree) so a reindexed ruling is the same ruling.
- **A renumbered rule keeps its calls.** Inside the CR load transaction, `renumber_map`
  (`ingest/src/renumber.rs`) matches old and new rules by body with every rule id masked
  (renumbering changes the cross-references too), only where the masked body is unique on
  both sides, then keeps only entries that reproduce the new rule exactly when the old one is
  rewritten with the whole map (a fixpoint, so a redirected cross-reference is not mistaken
  for a renumbering); `rewrite_call` then rewrites every call's `rule` citation ids, the ids
  inside `rule`/`prior_call` quotes and the answer text in one pass. Never guesses:
  ambiguous or reworded rules are left to the retirement pass. The CR loader and the
  retirement pass take the same advisory lock (`CALLS_REWRITE_LOCK`).
- **Agent sessions are the pipeline in pull mode, with the same validation.**
  `judge_bot::session` (`Session`/`Stage` machine, `Sessions` over `PgSessionStore`,
  table `agent_sessions`) hands an outside agent the extraction prompt, then the
  synthesis prompt rendered from the same `Context`, and admits its verdict only through
  `Verdict::validate`; one `lookup_rules` round, one retry, same rejection notice.
  `synth::system_prompt(Harness)` fills two tokens in `prompts/synth_system.md`
  (`{{LOOKUP_RULES}}`, `{{OUTPUT_FORMAT}}`); `Harness::Tool` is the tuned prompt the bot
  sends, and `harness_tests` pin its SHA-256 so a template edit that changes it fails a
  test until the digest is updated on purpose. Thread
  ids are `AgentThread` (`agent:<uuid>`, only mintable or parseable with the prefix) so a
  session can never read or write a Discord thread's history; inputs are bounded
  (`MAX_QUESTION_CHARS`, `MAX_EXTRACTION_ITEMS`, `MAX_LOOKUP_IDS`, `MAX_ANSWER_CHARS`);
  persisting is idempotent in the database (`calls.session_id` unique, `PersistCall`), and
  a session-persisted call is thread history only — the prior-call leg skips
  `session_id IS NOT NULL` rows because nothing can rate them.
  `Rejection` is adjacently tagged because it is stored. The surface is `crates/agent`
  (`ops.rs` is the one list of operations; `mcp.rs` and `bin/cli.rs` only transport),
  and `.claude/skills/judge/SKILL.md` tells Claude Code how to drive it.
- **Category taxonomy is data.** `data/categories.yaml` is the single source of truth;
  `crates/core/build.rs` generates the `Category` enum from it, so taxonomy edits are
  recompiles and matches stay exhaustive. The extractor's schema makes the primary
  category a required field (an empty classification is an API-level schema violation).
- **Gold eval set** (`eval/gold.yaml`): 21 adversarially verified questions with
  `expected_rule_ids` and per-question `equivalent_rule_ids` (alternate rule ids stating
  the same fact; keys must be quoted and present in the expected list — the loader
  enforces this). Extend it when adding capability; `rescore` re-grades old runs after
  gold edits. Rule ids written unquoted in YAML are rejected (floats drop trailing zeros).
- **Discord layer:** interaction logic is kept pure and unit-tested (`render.rs`,
  `ids.rs` typed button custom-ids, `pending.rs` did-you-mean store, `question.rs` span
  pinning). Replies open with a non-pinging `<@user> asked:` header; rule citations link
  to the Yawgatog CR mirror (anchor = `R` + id with dots stripped), rulings/Oracle text
  link to Scryfall search-by-oracleid (the `/card/<uuid>` route 404s).
- **Card symbols are pictures on both front doors.** `discord/mana.rs` substitutes
  Discord application emoji (`{W}` → `<:mana_w:…>`); `judge_core::symbol::emoji_name` is
  the one definition of the name — it lives in core precisely because two programs (the
  bot and the `ingest emoji` uploader) must agree on it exactly. Text is
  carried as `mana::Rendered` segments rather than a `String` because a tag costs ~28 of
  Discord's 2000/4096 characters and must never be cut in half — plain text is the only
  cuttable segment. An application with no emoji uploaded renders the literal `{W}`. The
  web page does the same job with Scryfall's SVGs (`web/src/Symbols.tsx`).

- **Providers are configuration, not code.** `judge_bot::config` loads `judge.toml`
  (`JUDGE_CONFIG`, else `./judge.toml` if present, else today's setup from `.env`:
  Anthropic direct, `claude-opus-5` both stages, Voyage if keyed) into typed structs
  (`deny_unknown_fields`, nutype validators, secrets by `api_key_env` read at load into a
  redacted `ApiKey`). Chat backends are `judge-anthropic` (`Endpoint::{Direct, Proxy}`; the
  cloud doors are named but refused as "not built") and `judge-openai` (chat completions
  with `Dialect` knobs: `structured_output`, `strict_tools`, `reasoning_effort`,
  `max_tokens_param`, `cache_hints`). A model on an `openai` provider must be priced
  (`[models.X.pricing]`) or the provider `pricing = "free"`; the built-in table errs high
  for unknown Anthropic models only. When a backend cannot enforce the output schema, the
  adapters append it to the *user turn* (`judge_llm::schema_block`) so the pinned system
  prompt digest and the Anthropic golden fixtures never change. Every binary logs
  `Config::summary()` at startup; `judge-cli config` prints the redacted resolution.

## Environment

`.env` (gitignored; template in `.env.example`): `DATABASE_URL` (port 5433),
`ANTHROPIC_API_KEY`, `VOYAGE_API_KEY` (blank = vector leg off, bot still works),
`JUDGE_CONFIG` (optional path to a `judge.toml`; see above — in Docker it is a path *inside*
the container, so mount the file: the compose file shows how; a `./judge.toml` in the repo
root is read by `cargo run` but is invisible to the containers),
`DISCORD_TOKEN`, `GUILD_ID` (instant command registration), `JUDGE_ROLE` (default
"Judge"), `JUDGE_MAX_USD`, `JUDGE_CONCURRENCY`; for the HTTP API also `API_ADDR`
(default `0.0.0.0:8787`), `WEB_DIST`, `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS`,
`API_CLIENT_IP` (`peer` or `cloudflare`; see below), `MCP_TOKEN` (unset = no `/mcp`; ≥24
chars; bearer-checked before the protocol), `MCP_ALLOWED_HOSTS` (the `Host` values the
MCP transport accepts — the public hostname behind the tunnel) and
`MCP_JUDGE_LIMIT`/`MCP_JUDGE_WINDOW_SECS` (`judge` runs per window through `/mcp`; the
blast radius of a leaked token). The
bot/api containers override `DATABASE_URL` to `db:5432` inside the compose network;
the image builds the web page and sets `WEB_DIST=/srv/web`.

Deployment is self-hosted behind a Cloudflare Tunnel — `docs/DEPLOYMENT.md` is the
runbook. `db` and `api` publish on `127.0.0.1` only; public traffic reaches `api:8787`
over the compose network from the `cloudflared` service, which the `tunnel` compose
profile starts (`COMPOSE_PROFILES=tunnel` in `.env`). Deploy credentials live in
`.env.deploy` (`TUNNEL_TOKEN`, `R2_*`), read only by `cloudflared` and
`scripts/backup-db.sh`, never by the internet-facing `bot`/`api`. Weekly
`scripts/backup-db.sh` dumps to R2 and has `list`/`fetch` subcommands for the restore
drill; restoring is far cheaper than re-ingesting, which re-pays Voyage per embedding.

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` (socket address) or `cloudflare` (`CF-Connecting-IP`); `client_ip` never reads
`X-Forwarded-For`, because Cloudflare *appends* to a caller-supplied header instead of
replacing it, making its first hop attacker-chosen — that would hand every request a
fresh allowance against a paid endpoint. The old `API_TRUST_FORWARDED` did exactly
that and is now rejected at startup rather than ignored. `cloudflare` is only sound
when nothing can reach the origin except Cloudflare.

**Data refresh is a nightly cron on the deploy host**, not a service: `scripts/refresh-data.sh`
runs the `refresh` compose service (profile `refresh`, third entrypoint `judge-ingest` in the
same image; `docker compose run` enables the profile itself so `up -d` never starts it). CR
release detection scrapes Wizards' rules page for the `MagicCompRules <date>.txt` link and
compares the date to the stored `cr_version`; the CR loader nulls embeddings only for rules
whose text changed, so a new CR costs Voyage a few hundred rules. `aliases` and `notes` are
not part of refresh — they are repo data, loaded when they change.
