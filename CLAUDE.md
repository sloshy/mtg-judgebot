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
- **The user is cost-sensitive on API spend.** Every Anthropic call goes through the
  spend-capped client (`JUDGE_MAX_USD`, default $5; the cap *reserves* worst-case cost
  before sending, so caps under ~$0.45 refuse synthesis outright). Develop against
  wiremock, not the live API; a full 21-question gold run costs ~$2.50.
- Commits in this repo are managed by Claude: commit completed, verified steps without
  asking. Never commit `.env`, `.cache/`, or `eval/runs/`.

## Commands

Everything needs env from `.env` (`set -a; source .env; set +a`). Postgres runs in
Docker on **localhost:5433** (a native Postgres owns 5432 — never touch it).

```sh
docker compose up -d                 # db (pgvector/pg16) + bot; both restart with Docker
docker compose up -d --build bot     # redeploy the bot after code changes
cargo build --workspace
cargo clippy --workspace --all-targets   # must be warning-free; lints deny unwrap/expect/indexing/panic
cargo test --workspace               # includes #[sqlx::test] suites that spin temp DBs off DATABASE_URL
cargo test -p judge-bot possessive   # run a single test by substring
SQLX_OFFLINE=true cargo build --workspace   # must pass; regenerate .sqlx after SQL changes:
cargo sqlx prepare --workspace -- --all-targets

~/.cargo/bin/sqlx migrate run --source crates/bot/migrations
cargo run --release -p judge-ingest -- cards            # Scryfall bulk sync (cached in .cache/)
cargo run --release -p judge-ingest -- rules <url|path> # CR parse; current CR url is in docs/ARCHITECTURE.md
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed            # only rows with NULL embedding; Voyage

cargo run -p judge-eval -- recall                       # retrieval gate, no API keys, exit≠0 below 90%
cargo run -p judge-eval -- answer --label L --limit 21 --max-usd 6.00   # full live gold run (~$2.50)
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
`anthropic` (hand-written wire client: we own the API types; schemars → Anthropic's
schema subset via a transform that must keep `additionalProperties:false` and rewrite
`oneOf→anyOf`) ← `embed` (Voyage) ← `bot` (sqlx adapters, prompts in
`crates/bot/src/prompts/`, serenity/poise Discord layer with pure `render.rs`) and
`ingest` / `eval` (bins). `judge_bot::build_deps` is the single composition root shared
by the bot and eval.

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
  with ≥5 votes are excluded, stale-CR calls are excluded, and prior calls are always
  rendered *after* CR material as examples.
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

## Environment

`.env` (gitignored; template in `.env.example`): `DATABASE_URL` (port 5433),
`ANTHROPIC_API_KEY`, `VOYAGE_API_KEY` (blank = vector leg off, bot still works),
`DISCORD_TOKEN`, `GUILD_ID` (instant command registration), `JUDGE_ROLE` (default
"Judge"), `JUDGE_MAX_USD`, `JUDGE_CONCURRENCY`. The bot container overrides
`DATABASE_URL` to `db:5432` inside the compose network.

There is no scheduled data refresh yet; Scryfall/CR ingest is manual (see Commands).
