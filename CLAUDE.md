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
- **Every stage boundary gets an adversarial subagent review.** At the end of each phase
  of a multi-step build, and before each commit, hand the change to a subagent briefed to
  *try to break it*: name the invariant the change claims to establish, point it at the
  diff and the code paths that consume the changed data, and ask for concrete failure
  scenarios (input → wrong behaviour) separated into confirmed and plausible, with an
  explicit "nothing severe" rather than manufactured findings. Fix what it confirms, then
  commit. The reviewer inherits this session's model unless the user names another one.

## Reproducing a reported failure

When the user brings a bad or failed answer to troubleshoot, reach for the cheapest
harness that can reproduce it, in this order. The rule is about *how to drive the
pipeline*, not about how much to investigate.

1. **The `judge` skill, session mode** (`.claude/skills/judge/SKILL.md`, driven through
   `target/release/judge-cli begin|extract|rules|verdict`). Claude is the model, so there
   is no API spend and no model configuration to get right, and the extraction, the
   rendered material, the citation validation and the rejection notices are the same code
   the bot runs. Almost every report — a rejected citation, a wrong card resolution, a
   thin or missing context, an unhelpful retry notice — is reproducible here, so start
   here and stay here. It needs Postgres and nothing else, so check `docker compose ps`
   first and bring the database up if it is down — `docker compose up -d db`, which is
   enough on its own: neither `bot` nor `api` has to be running.
2. **MCP** — `judge-mcp` over `.mcp.json` locally when the tools are connected, or
   `judge-api`'s `/mcp` (with `MCP_TOKEN`) when the user is away from this machine and the
   local database is not reachable. Same operations as the CLI; prefer whichever transport
   is actually available.
3. **A live instance** (`docker compose up -d bot api`, `judge-cli judge`, `judge-eval
   answer`) only when the deployed surface itself is what's in question — Discord
   rendering, buttons, rate limiting, startup/config, the spend cap — or when step 1 has
   ruled the pipeline out. This spends real money on real model calls, so say what it will
   cost before starting it.

**The exception is a question about another model or provider.** When the report is
"Gemini/GPT/this endpoint answers badly through the bot", the model's own behaviour is the
thing under test and Claude-as-the-model reproduces nothing. Run the real pipeline against
that provider's `judge.toml` (`JUDGE_CONFIG=… judge-cli judge`, `judge-eval answer
--config <file>`, or the containers with that file mounted), which also assumes the user
has that provider set up; ask for the config rather than inventing one. The same applies
to anything provider-shaped: wire format, schema dialect, pricing, auth.

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

cargo run --release -p judge-ingest -- migrate          # apply pending migrations explicitly (judge_bot::MIGRATOR);
                                                        # bot/api do this at startup unless JUDGE_AUTO_MIGRATE=false
~/.cargo/bin/sqlx migrate run --source crates/bot/migrations   # the same thing with sqlx-cli
cargo run --release -p judge-ingest -- cards            # Scryfall bulk sync (cached in .cache/)
cargo run --release -p judge-ingest -- rules <url|path> # CR parse from a given file or URL
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed            # only rows with NULL embedding; the configured
                                                        # embedder ([models.embed] or VOYAGE_API_KEY); refuses
                                                        # if embedding_space or the columns' width differ
cargo run --release -p judge-ingest -- reembed [--yes] [--clear]  # make the DB hold the configured embedder's
                                                        # space: when it holds another (row or column width),
                                                        # retype vector columns, rebuild HNSW, NULL every
                                                        # vector, rewrite embedding_space, then embed all; when
                                                        # it already holds it, only fill empty rows (idempotent;
                                                        # resume an interrupted refill with it). --clear clears
                                                        # and re-pays every row in the same space. Without --yes:
                                                        # prints rows + rough cost, exit≠0, changes nothing.
                                                        # `docker compose restart bot api` after a switch
                                                        # (`up -d` sees no change: the file is a mount).
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

cargo run -p judge-eval -- recall [--vectors]           # retrieval gate, no API keys (--vectors: the configured
                                                        # embedder, ~$0.001), exit≠0 below 90% retrieved or 75%
                                                        # shown under the synthesis budget
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
unioned in priority order: the primary category's CR subsections ranked by text relevance
(those sharing no word with the question after the next two legs), tsvector BM25, pgvector
cosine, the secondary categories — the synthesis budget renders a
prefix, so this order is what the model reads; plus rulings for all faces, glossary, nightmare-card notes, rated prior calls) →
synthesis (high-effort, one `lookup_rules` tool round max — enforced by typestate — with
citation validation and one retry) → persist + Discord rating buttons.

Crate graph: `core` (domain ADTs, ports, `judge()`, citation validation — pure) ←
`llm` (the provider seam, `docs/proposals/providers.md`: neutral `ChatRequest`/`ChatResponse`,
the open `Backend` trait providers implement and the sealed `ChatModel` port the pipeline
calls — only `Metered<B>` implements it, so every send is behind the spend cap by type;
`SpendMeter` + `Price::{Free, Table, PerToken}` (a table price is re-read for the model the
response names; an operator's `PerToken` settles at exactly its rate), the shared HTTP
retry loop, the `Synth` typestate and `classify`) ← `anthropic` (a `Backend`: hand-written
wire types we own, `Endpoint` enum, neutral↔wire conversion; schemars → Anthropic's schema
subset via a transform that must keep `additionalProperties:false` and rewrite
`oneOf→anyOf`, applied at conversion time) and `openai` (a `Backend` for chat completions:
its own wire types, the strict-schema transform — every property `required`, optionals
`anyOf [T, null]` — string tool arguments parsed by serde, `choices[0].message` replayed
verbatim) ← `embed` (Voyage + OpenAI-compatible `/embeddings`, each a `WithSpace`) ←
`bot` (sqlx adapters, `config.rs` = the `judge.toml` loader, `extract.rs`/`synth.rs` over
`judge-llm` only, prompts in `crates/bot/src/prompts/`, serenity/poise Discord layer with
pure `render.rs`) and `ingest` / `eval` / `api` (bins) and `agent` (lib + `judge-cli` /
`judge-mcp` bins; `api` mounts its MCP handler). `judge_bot::build_deps(pool, Models,
embedder)` is the single composition root shared by the bot, eval, the HTTP API and the
agent's `judge` tool; `Models::{single, pair, priced}` take the meter and bare backends and
meter them themselves (private fields: no uncapped model, no foreign meter). Every binary
gets its `Models`/`Vectors` from `config::Config::load()`, whose no-file branch
(`from_vars`) is the zero-config setup (Anthropic direct, one model for both stages, one
`SpendMeter`). `crates/bot/tests/anthropic_golden.rs` pins the four Anthropic request shapes
byte-for-byte against captured fixtures (`UPDATE_GOLDEN=1` re-captures them after an
intended prompt/schema change; review the diff). `api` (+ the SolidJS page in `web/`) is the
anonymous front door: no ratings, stateless "did you mean?" via `pins` → `pin_card`
rewriting, session history via a client UUID (`web:<uuid>` thread ids), per-IP
fixed-window rate limiting (`API_RATE_LIMIT`/`API_RATE_WINDOW_SECS`) ahead of the
concurrency semaphore and the spend cap.

Key cross-file facts that aren't obvious from any one file:

- **Citations are typed and validated.** `Citation::{Rule, ScryfallRuling, OracleText,
  PriorCall}` each carry a verbatim `quote` checked as a substring of the source in
  `Context`. The check folds typographic punctuation (`judge_core::quote`: curly quotes,
  the dash block, non-breaking spaces — one `char` to one `char`, never case or words),
  because models retype the CR's `’` as `'` and that was the most common rejection; what
  is stored is the *source's* span, not the model's string, so a persisted quote stays
  byte-exact and the retirement pass's `citation_supported` stays a strict check. A failed
  check (or an empty/citation-less verdict on an answerable source) becomes a retry with
  the rejection rendered into the prompt (logged at INFO, so a second failure can be read
  against the first). Only `Verdict<Validated>` can reach `CallStore::persist` or Discord
  rendering.
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
- **Vectors carry their space, and the database records the one it holds.** Every embedder
  (`judge_embed::{VoyageEmbedder, OpenAiEmbedder}`) implements `WithSpace`: a `Space`
  (provider *kind* `voyage|openai`, model, dimensions). The one-row table `embedding_space`
  (migration `20260904000001`, seeded `voyage/voyage-3.5/1024` for a DB that already held
  vectors) names what the stored vectors are; `Space::check` (pure, in `judge_embed::space`) is
  the only definition of "same space". `ingest embed` writes the row on first use, refuses on a
  mismatch or when the columns' actual `vector(N)` typmod differs (`db/space.rs`
  `column_width`), and never relabels vectors it did not write. The adapters hold no bare
  `Embedder`: `PgRetriever`/`PgLibrary`/`PgCallStore` take `Arc<db::Vectors>`
  (`Config::vectors(pool)`, one per process), which embeds nothing until the stored space
  equals its own — a mismatch is an error-level log naming both spaces and dark vector legs,
  never a mixed column. The row is re-read on every use (and once at startup, so the verdict
  sits beside the config summary): a running bot picks up the first `ingest embed`, and a
  `reembed` under it darkens the legs instead of erroring or mixing. Writers hold the space:
  `PgCallStore::persist` and every `ingest embed` batch take the shared side of
  `CALLS_REWRITE_LOCK` in their transaction and read the row under it (`hold_space` /
  `Vectors::hold`), `switch_space` takes the exclusive side (as the CR loader and the
  retirement pass do), so a switch waits for in-flight writes and a write after it sees the
  new row. `ingest reembed --yes` (`switch_space`) is the only thing that changes the row and
  the column width, in one transaction, then runs the embed loop; it probes the embedder
  first (one short text) so a wrong key/URL/model/width fails before anything is cleared;
  the HNSW index definitions it recreates live beside it in `VECTOR_TABLES`, verbatim from
  the migrations. `config::Dimensions` is `1..=2000` (HNSW's limit) at load. A
  `[providers.X] kind = "openai"` table serves embeddings too (`[models.embed]` needs
  `dimensions` there; `send_dimensions = false` for servers that reject the field).
  `ingest embed`/`refresh` load the same `Config`, so a deployment with a `judge.toml` mounts
  it into `refresh` as well.
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
  Anthropic direct, `claude-opus-5` both stages, Voyage if keyed; `judge.example.toml`
  documents every knob with its default and must keep loading — `config::tests::the_example_file_loads_as_shipped`
  and `..._with_every_door_uncommented` pin that, so a renamed knob fails the gate)
  into typed structs (`deny_unknown_fields`, nutype validators — `BaseUrl`, `Region`,
  `Project`, `WorkspaceId`, `Dimensions` — secrets by `api_key_env` read at load into a
  redacted `ApiKey`; a knob that would be ignored is an error naming both keys). Chat
  backends are `judge-anthropic` (`Endpoint::{Direct, Proxy, ClaudePlatformOnAws, Bedrock,
  Vertex}`; the cloud doors sit behind judge-anthropic's `aws`/`gcp` Cargo features —
  default on, forwarded from judge-bot's own features, named in the Dockerfile — so a lean
  build cannot even name them and the loader says "not built"; their credentials come from
  the platform chains (SigV4 via aws-config — service `aws-external-anthropic` with the
  `anthropic-workspace-id` header, or `bedrock-mantle` — and ADC via gcp_auth), never
  `judge.toml`, resolved lazily and probed once at startup by `Config::probe_auth` so an
  empty chain fails there, not per question; one `Endpoint` per provider table, shared by
  the stages naming it; Proxy/Vertex mask the `fallbacks` beta, Bedrock also masks
  `output_config.format`, tool `strict` and every `anthropic-beta`, verified against the
  live docs 2026-09-02) and `judge-openai` (chat completions with `Dialect` knobs:
  `structured_output`, `strict_tools`, `reasoning_effort`, `max_tokens_param`,
  `cache_hints`; `send_dimensions` for embeddings). A model on an `openai` provider must be
  priced (`[models.X.pricing]`, cache prices defaulting high from `input`) or the provider
  `pricing = "free"`; the built-in table (`judge_llm::PRICES`) errs high for unknown
  Anthropic models only. When a backend cannot enforce the output schema, the adapters
  append it to the *user turn* (`judge_llm::schema_block`) so the pinned system prompt
  digest and the Anthropic golden fixtures never change. Every binary logs
  `Config::summary()` at startup; `judge-cli config` prints the redacted resolution;
  `eval answer --config` records `provider/model` per stage in the run file.

## Environment

`.env` (gitignored; template in `.env.example`): `DATABASE_URL` (port 5433),
`ANTHROPIC_API_KEY`, `VOYAGE_API_KEY` (blank = vector leg off, bot still works; a `judge.toml`
`[models.embed]` overrides it, including OpenAI-compatible embeddings),
`JUDGE_CONFIG` (optional path to a `judge.toml`; see above — a *host* path: `cargo run`
reads it as is, and `docker-compose.yml` bind-mounts it into `bot`/`api`/`refresh` at
`/etc/judgebot/judge.toml` and points their `JUDGE_CONFIG` there (`${JUDGE_CONFIG:+…}`;
blank mounts the tracked example, which nothing reads — so a `./judge.toml` in the repo
root is read by `cargo run` but invisible to the containers until `JUDGE_CONFIG` names
it; and editing the mounted file's content is not a change `up -d` recreates for, so
`docker compose restart bot api`); the `api_key_env` of every provider a stage names
lives in `.env` too (a table no stage names is parsed, its key never read), as do the
cloud doors' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS` — never `.env.deploy`, which
`bot`/`api` do not read),
`DISCORD_TOKEN`, `GUILD_ID` (instant command registration), `JUDGE_ROLE` (default
"Judge"), `JUDGE_MAX_USD`, `JUDGE_CONCURRENCY`, `JUDGE_AUTO_MIGRATE` (default true: bot and
api apply pending migrations at startup; `judge-ingest migrate` is the explicit form); for the HTTP API also `API_ADDR`
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
drill; restoring is far cheaper than re-ingesting, which re-pays the embedder per row —
take one before `ingest reembed --yes` (runbook in `docs/DEPLOYMENT.md` §7).

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
whose text changed, so a new CR costs the embedder a few hundred rules. `aliases` and
`notes` are not part of refresh — they are repo data, loaded when they change.
