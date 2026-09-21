# Contributing

This page covers the development environment, the gates a change has to pass, and the
design rules that are not obvious from the code. `docs/EXPLAINER.md` explains how the bot
works end to end. `docs/ARCHITECTURE.md` is the design reference.

## Development setup

You need Docker (with the compose plugin), Rust 1.98 (`rust-toolchain.toml` pins it,
so `rustup` installs it on first use) and Node 24 for the web page.

```sh
cp .env.example .env            # DATABASE_URL already points at the compose database
docker compose up -d db         # pgvector Postgres on localhost:5432
set -a; source .env; set +a     # optional: the binaries read .env themselves; this puts it in your shell too
cargo build --workspace
cargo test --workspace          # the #[sqlx::test] suites create throwaway databases off DATABASE_URL
```

The database publishes on loopback port **5432**. If something on your machine already
has it, set `DB_PORT` in `.env` and change the port in `DATABASE_URL` to match. The
compose file reads the same variable.

Nothing in the test suite calls a paid API. HTTP backends are tested against `wiremock`.
Develop against it too, and keep `ANTHROPIC_API_KEY` blank unless you mean to spend.
Card and rules data is needed to run the pipeline, not to build or test.
`cargo run --release -p judge-ingest -- init` loads all of it from source (the README's
"Running it" does the same in a container), and every `docker compose run --rm refresh
<command>` in the docs is `cargo run --release -p judge-ingest -- <command>` here.

## The gates

CI (`.github/workflows/ci.yml`) runs these on every pull request, and the image is
published only from a tree that passes them. `scripts/check.sh` runs the same gates
locally, and git hooks run it for you:

```sh
scripts/dev-setup.sh      # once per clone: the pinned linters, npm ci in web/ and site/, the hooks
```

Both hooks check exactly what is being committed or pushed, not your working tree. They
check it out into a scratch worktree under `.git/`, which shares `target/` and the
linters with your clone, has its own `node_modules` (reinstalled when a lockfile
changes) and gets `.env.example` as its `.env`, as CI does. The `.sqlx` check migrates a
throwaway `judgebot_check` database on your server, never your development one. An unstaged fix, a file you forgot to add or another branch's state cannot make
them pass.

- **pre-commit** checks the staged tree. It runs formatting and clippy when Rust or
  compiled-in data is staged, Biome and the build for `web/`, and Biome, `astro check`,
  the build and a link check for the docs site. It always runs the repository lints:
  `cargo deny` (against the advisory database already fetched, so it works offline),
  `cargo machete`, `taplo`, `typos`, `shellcheck`, `actionlint`, `hadolint` and the
  compose file. It takes seconds unless clippy has a lot to recompile.
- **pre-push** checks the tip of each ref being pushed, with the groups the push
  changes since the remote's commit. Changed Rust, compiled-in data, `eval/` or the
  compose file also runs `cargo test`, the migrations and the `.sqlx` freshness check,
  which need the database up (`docker compose up -d db`) and `sqlx-cli`. A change to
  `scripts/check.sh`, `scripts/tools.sh` or a workflow, a new branch, or a remote
  commit your clone has not fetched gets every group. CI runs the same script, so a
  push that gets past it on top of a commit that passed CI passes CI too.

`scripts/check.sh rust test` (any of `rust sqlx test web site lint`) runs chosen
groups on the working tree, and `scripts/check.sh --at <rev>` on a commit. `git commit --no-verify` / `git push --no-verify` skip a hook once. The
linters are prebuilt binaries at the versions in `scripts/tools.sh`, which CI installs
too. They live in `.tools/` (gitignored), and bumping a version there is the whole
upgrade. `npm --prefix web run fix` (or `site`) applies Biome's formatting and safe
fixes, and `.tools/bin/taplo fmt` formats the TOML.

Workspace lints deny `unwrap`, `expect`, indexing and `panic!` in every crate, and warn
on missing docs and the pedantic group. Reach for a type or a `Result` instead of a
suppression. When one is right, keep it on the one item as
`#[expect(lint, reason = "…")]`. A bare `#[allow]` is a build error, and an `expect`
that stops matching anything fails the build too, so a stale suppression cannot linger.

`deny.toml` is the dependency policy. It covers RustSec advisories, the licences an
AGPL image may carry, and crates.io as the only source. A dependency that brings a
new licence fails CI until the licence is added there. An accepted advisory is listed
there with its reason.

**SQL changes.** Queries are checked at compile time against the schema. After editing
any `sqlx::query!` or migration, regenerate the committed offline data and include it
in the same commit. CI fails when it is stale.

```sh
cargo sqlx prepare --workspace -- --all-targets   # needs the database up and migrated
SQLX_OFFLINE=true cargo build --workspace          # must pass without a database
```

Install the tool with `cargo install sqlx-cli --no-default-features --features
postgres,rustls` if you do not have it.

**Prompt and schema changes.** Two tests pin the bytes the bot sends. If you edit a
prompt template under `crates/bot/src/prompts/` or a struct that feeds the model's output
schema, both fail on purpose:

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
float and is rejected). `cargo run -p judge-eval -- recall` is the free retrieval gate.
`rescore` re-grades stored runs after a gold edit without spending anything.

## Design rules

The full statement is `docs/DECISIONS.md` D1. These are the ones a change is most
likely to bump into.

- **Make the bad state unrepresentable.** Invariants live in types: closed enums,
  validated newtypes, `NonEmpty`, and typestates such as `Verdict<Unvalidated>` →
  `Verdict<Validated>` and the synthesis loop's single tool round. Prefer that over a
  runtime check, and a runtime check over a test.
- **`crates/core` has no I/O.** No reqwest, sqlx, tokio-net or provider SDK may be added
  to it. That dependency fence stands in for effect tracking.
- **Every model call is metered.** The only way to send a request is through
  `judge_llm::Metered`, which reserves the worst-case cost against `JUDGE_MAX_USD`
  first. Do not add a second path to a provider.
- **Citations are validated, never trusted.** A quote must be a substring of its source in the
  retrieved context. Only a `Verdict<Validated>` can be persisted or shown.
- **The resolver never guesses.** Ambiguity in a card name becomes a "did you mean"
  prompt, not a best effort.
- **Secrets are `ApiKey`.** Anything that authenticates (provider keys, the Discord
  token, the MCP bearer token) is wrapped so a `Debug` of any config prints
  `<redacted>`. A user's question text does not go to logs above `debug`. The card
  names extracted from it do.

## Debugging a wrong answer

The cheapest way to reproduce a bad ruling is the agent session. It runs the bot's
extraction, retrieval and citation validation with *you* as the model, and it spends
nothing. Build `judge-cli` (`cargo build --release -p judge-agent`), bring up the
database, and step through `judge-cli begin | extract | rules | verdict` as
`.claude/skills/judge/SKILL.md` describes. `judge-cli card`, `search`, `get-rules` and
`glossary` query the database directly. Reach for `judge-cli judge` or a live bot only
when the deployed surface is in question. Those calls cost money.

## Pull requests

- Keep a pull request to one change, with the reason in the description. Tests and
  the `.sqlx` or golden fixtures it needs go in the same commit as the code.
- The changelog is `CHANGELOG.md`, Keep-a-Changelog style. Add a line under
  *Unreleased* for anything an operator or user would notice.
- Card names, rules text and rulings in tests should be short excerpts. Do not commit
  data dumps, `.env` files, `judge.toml` or anything under `eval/runs/`.

## Releases

A release is a GitHub release whose tag is `vX.Y.Z` on a commit of `main`. Move the
*Unreleased* changelog section under that version first, then create the release
(`gh release create vX.Y.Z --generate-notes`, or the web form). Publishing it runs
`publish-image.yml`, which tags the image already built for that commit as `X.Y.Z`,
`X.Y` and, from 1.0 on, `X`, without rebuilding. The release is the image that has
been running as `latest`, down to the platform digests. Wait for the push's *Publish
image* run to finish before publishing the release, or the two build in parallel. A
commit with no image of its own (a docs-only commit, or a build a later push
cancelled) is built first, through CI. Pre-release tags such as
`v1.0.0-rc.1` get only their exact version tag. `latest` keeps following `main`.

## Claude Code

The repository carries a `.mcp.json` and a `judge` skill under `.claude/skills/`. If
you use Claude Code, they let it drive the judge pipeline as the model through
`judge-mcp`. They are inert otherwise. `CLAUDE.md` is Claude's working notes for this
codebase. It is kept in sync with the docs, but the docs are the source of truth.
