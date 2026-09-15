# Contributing

Thanks for looking at this. This page covers getting a development environment up,
the gates a change has to pass, and the design rules that are not obvious from the
code. `docs/EXPLAINER.md` explains how the bot works end to end; `docs/ARCHITECTURE.md`
is the design reference.

## Development setup

You need Docker (with the compose plugin), Rust 1.97 (`rust-toolchain.toml` pins it,
so `rustup` installs it on first use) and Node 22 for the web page.

```sh
cp .env.example .env            # DATABASE_URL already points at the compose database
docker compose up -d db         # pgvector Postgres on localhost:5433
set -a; source .env; set +a     # every cargo command below reads .env from the environment
cargo build --workspace
cargo test --workspace          # the #[sqlx::test] suites create throwaway databases off DATABASE_URL
```

The database publishes on **5433**, not 5432, so it never collides with a Postgres
already installed on the host. If you change it, change both `DATABASE_URL` in `.env`
and the port mapping in `docker-compose.yml`.

Nothing in the test suite calls a paid API. HTTP backends are tested against
`wiremock`; develop against it too, and keep `ANTHROPIC_API_KEY` blank unless you are
deliberately spending. Loading real card and rules data (`README.md`, "Running it") is
only needed to run the pipeline for real, not to build or test.

## The gates

CI (`.github/workflows/ci.yml`) runs these on every pull request, and the image is only
published from a tree that passes them. Run them locally first:

```sh
cargo fmt --all --check
SQLX_OFFLINE=true cargo clippy --workspace --all-targets   # must be warning-free
cargo test --workspace
npm --prefix web ci && npm --prefix web run build
```

Workspace lints deny `unwrap`, `expect`, indexing and `panic!` in every crate, and warn
on missing docs and the pedantic group. Reach for a type or a `Result` instead of an
`#[allow]`; when an allow is right, keep it on the one item and say why in a comment.

**SQL changes.** Queries are checked at compile time against the schema. After editing
any `sqlx::query!` or migration, regenerate the committed offline data and include it
in the same commit; CI fails when it is stale:

```sh
cargo sqlx prepare --workspace -- --all-targets   # needs the database up and migrated
SQLX_OFFLINE=true cargo build --workspace          # must pass without a database
```

Install the tool with `cargo install sqlx-cli --no-default-features --features
postgres,rustls` if you do not have it.

**Prompt and schema changes.** Two tests pin the exact bytes the bot sends. If you
edited a prompt template under `crates/bot/src/prompts/` or a struct that feeds the
model's output schema, both will fail on purpose:

- `crates/bot/tests/anthropic_golden.rs` compares four request bodies to fixtures. Run
  it with `UPDATE_GOLDEN=1` to re-capture them, then read the diff of the fixtures as
  part of your review.
- `harness_tests` in `crates/bot/src/synth.rs` pins the SHA-256 of the tuned synthesis
  system prompt. Update the digest in the test when the change is intended.

**Taxonomy changes.** `data/categories.yaml` generates the `Category` enum at build
time (`crates/core/build.rs`), so editing it is a recompile, and every `match` over the
enum has to be updated. `data/aliases.yaml` and `data/notes.yaml` are loaded into the
database by `judge-ingest`, not compiled in.

**Gold set changes.** `eval/gold.yaml` quotes every rule id (unquoted `702.10` is a
float and is rejected). `cargo run -p judge-eval -- recall` is the free retrieval gate;
`rescore` re-grades stored runs after a gold edit without spending anything.

## Design rules

The full statement is `docs/LANGUAGE_EVALUATION.md` §1; these are the ones a change is
most likely to bump into.

- **Make the bad state unrepresentable.** Invariants live in types: closed enums,
  validated newtypes, `NonEmpty`, and typestates such as `Verdict<Unvalidated>` →
  `Verdict<Validated>` and the synthesis loop's single tool round. Prefer that over a
  runtime check, and a runtime check over a test.
- **`crates/core` has no I/O.** No reqwest, sqlx, tokio-net or provider SDK may be added
  to it. That dependency fence is what stands in for effect tracking.
- **Every model call is metered.** The only way to send a request is through
  `judge_llm::Metered`, which reserves the worst-case cost against `JUDGE_MAX_USD`
  first. Do not add a second path to a provider.
- **Citations are validated, never trusted.** A quote must be a substring of its source
  in the retrieved context; only a `Verdict<Validated>` can be persisted or shown.
- **The resolver never guesses.** Genuine ambiguity in a card name becomes a "did you
  mean" prompt, not a best effort.
- **Secrets are `ApiKey`.** Anything that authenticates (provider keys, the Discord
  token, the MCP bearer token) is wrapped so a `Debug` of any config prints
  `<redacted>`. A user's question text does not go to logs above `debug`; the card
  names extracted from it do.

## Debugging a wrong answer

The cheapest way to reproduce a bad ruling is the agent session, which runs the real
extraction, retrieval and citation validation with *you* as the model and spends
nothing: build `judge-cli` (`cargo build --release -p judge-agent`), bring up the
database, and step through `judge-cli begin | extract | rules | verdict` as
`.claude/skills/judge/SKILL.md` describes. `judge-cli card`, `search`, `get-rules` and
`glossary` query the database directly. Only reach for `judge-cli judge` or a live bot
when the deployed surface itself is in question; those calls cost money.

## Pull requests

- Keep a pull request to one change, with the reason in the description. Tests and
  the `.sqlx` or golden fixtures it needs go in the same commit as the code.
- The changelog is `CHANGELOG.md`, Keep-a-Changelog style; add a line under
  *Unreleased* for anything an operator or user would notice.
- Card names, rules text and rulings in tests should be short excerpts. Do not commit
  data dumps, `.env` files, `judge.toml` or anything under `eval/runs/`.

## Claude Code

The repository carries a `.mcp.json` and a `judge` skill under `.claude/skills/`. If
you use Claude Code, they let it drive the judge pipeline as the model through
`judge-mcp`; they are inert otherwise. `CLAUDE.md` is Claude's working notes for this
codebase and is kept in sync with the docs, but the docs are the source of truth.
