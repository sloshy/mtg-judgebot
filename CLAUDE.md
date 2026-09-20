# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project values

- **Compiler enforcement over test discipline.** This project chose Rust so invariants
  live in types: exhaustive enums, nutype newtypes with validators, `NonEmpty`, and
  typestate (`Verdict<Unvalidated|Validated>`, `Synth<Fresh|ToolRequested|Final>`).
  When adding an invariant, prefer making the bad state unrepresentable. A runtime check
  or test is the fallback. `docs/DECISIONS.md` D1 lists the nine invariants (I1–I9) the
  design is built around. The rest of that file records every load-bearing decision and the
  alternative it rejected.
- **`crates/core` has no I/O dependencies.** That dependency-graph fence stands in for
  effect tracking, the one thing Rust doesn't give us. Never add reqwest/sqlx/tokio-net
  to core.
- **The user is cost-sensitive on API spend.** Every model call goes through the
  spend-capped `judge_llm::Metered` (`JUDGE_MAX_USD`, default $5). `JUDGE_BUDGET_PERIOD`
  (`process` | `day` | `month`) says what the cap covers (D19): `judge_bot::budget` keeps
  the `spend_days` ledger every 10 s and sets the meter's *adjustment*, so `bot` and `api`
  share one period total that survives restarts. The meter itself stays storage-free.
  `JUDGE_ALERT_WEBHOOK` is told when the cap trips and when a cron script fails
  (`scripts/alert.sh`). `judge-cli stats` reads the ledger. The cap *reserves*
  worst-case cost before sending, so caps under ~$0.45 refuse synthesis outright. Develop
  against wiremock, not the live API. A full 21-question gold run costs ~$2.50.
- Claude manages commits in this repo: commit completed, verified steps without asking.
  Never commit `.env`, `.cache/`, or `eval/runs/`.
- **Every stage boundary gets an adversarial subagent review.** At the end of each phase
  of a multi-step build, and before each commit, hand the change to a subagent briefed to
  *try to break it*:
  - Name the invariant the change claims to establish.
  - Point it at the diff and the code paths that consume the changed data.
  - Ask for concrete failure scenarios (input → wrong behaviour), separated into
    confirmed and plausible, with an explicit "nothing severe" rather than manufactured
    findings.

  Fix what it confirms, then commit. The reviewer inherits this session's model unless
  the user names another one.

## Public-facing docs

`docs/*.md` stay the canonical sources. The code and this file point at them by path.
The documentation site is `site/` (Astro + Starlight). `site/scripts/sync-docs.mjs` copies
each canonical file into `site/src/content/docs/` with Starlight frontmatter. It can also
copy a range of a file's numbered `## N.` sections, or a README section between two
headings. The copies are gitignored build output. The manifest at the top of the script is
the one list, and the sync script deletes copies whose manifest entry is gone.

Pages with no canonical file are authored directly under `site/src/content/docs/`: what
the judge is, trying it without Discord, requirements and first run, Discord app setup,
configuration reference, Discord commands, agents, command reference, data files,
attribution, schema.

`npm --prefix site run build` runs the sync first. `publish-docs.yml` deploys `site/dist`
to GitHub Pages on pushes touching the sources.

`docs/` holds five files: `ARCHITECTURE.md` (what exists), `DECISIONS.md` (why, D1–D19),
`PROVIDERS.md` (the model-provider reference), `DEPLOYMENT.md`, `EXPLAINER.md`. Retired
proposals live in git history only.

- **One judgebot per community** (D16). The docs teach an operator to create their own
  Discord application. There is no tenancy layer and none is planned.
- **No instance is ever named.** There is no bot to invite, no "try it" link and no
  running deployment referenced anywhere in the repository or the site. That includes a
  hostname and "the maintainer's instance". The reader's own instance is the only one
  that exists.
- The prose is reference documentation, so it opens on the subject, never on a greeting
  ("Thanks for looking at this", "We hope…", "Feel free to…").
- Internal links are relative (`../../using/discord/`) so `SITE_BASE` can change. The
  exception is `site/src/components/`, which renders at every depth and uses
  `import.meta.env.BASE_URL`.
- When a fact changes in code, fix it in the canonical doc *and* in any authored page that
  repeats it. `CONTRIBUTING.md`, `SECURITY.md` and `CHANGELOG.md` are synced too.

The site's header is Starlight's, with two changes. Both serve the splash pages (the
landing page and the 404), which have no sidebar and so neither a sidebar nav nor a mobile
menu button.

- `components.SocialIcons` is overridden by `site/src/components/HeaderLinks.astro`, the
  "Docs" link into the first docs page. Starlight has no top nav of its own.
- `site/src/styles/custom.css` hands a splash page's header the four variables
  Starlight's header grid reads off a *docs* page (`--sl-content-inline-start`,
  `--__sidebar-pad`, `--__toc-width: initial`, `--sl-content-width`). The search box then
  sits where the docs put it, aligned with the article text, instead of 98px to its left
  at 1440 and 156px from 1600 up. A second rule keeps the link (and only the link) visible
  below 50rem, where Starlight hides the header's right-hand group. A third takes it back
  out of the printed page.

`components.Footer` is overridden by `site/src/components/Footer.astro`: Starlight's own
footer, then the Fan Content Policy statement and the data sources (Wizards, Scryfall,
Yawgatog). Starlight renders the footer on splash pages too, so it is on every page. The
web page's footer (`web/src/App.tsx`) carries the same text; change them together.

**The icon** is 32x32 pixel art and must stay pixelated, so it is a PNG, never an SVG or a
smoothed resize. `assets/icon.png` is the 512px original (the README, a Discord app's
avatar). Copies: `site/src/assets/icon.png` (header logo) and `site/public/icon.png`
(`og:image`). `favicon.png` (the native 32px grid) and `apple-touch-icon.png` (192px,
nearest-neighbour, on `#1f2933`) sit in both `site/public/` and `web/public/`. Regenerate
every copy from the original when it changes, and scale only by whole multiples of 32.
Wherever it is displayed scaled, the CSS sets `image-rendering: pixelated`.

Check site changes in a browser with the `playwright-cli` skill. Run it against
`npm --prefix site run dev` (port 4321, base `/`) or a preview of the build (base
`/mtg-judgebot`, which is what Pages serves). Take the page at 1920, 1440, 820 and 390
wide, on a splash page *and* a docs page. Read positions out of
`getBoundingClientRect()` rather than by eye.

CI is `.github/workflows/ci.yml`: fmt, clippy, `.sqlx` freshness, tests on a pgvector
service, web and site builds, compose parse. It also runs Biome over `web/` and `site/`
(`biome.json` at the root, `.astro` files left to `astro check`), `astro check`, and
lychee over the built site's internal links. A `lint` job runs `cargo deny check`
(`deny.toml`), `cargo machete`, `taplo fmt --check` (`.taplo.toml`, which leaves out
`judge.example.toml`'s hand-aligned comments), `typos` (`_typos.toml`), shellcheck,
actionlint and hadolint (`.hadolint.yaml`). Every tool's config records why each
ignore is there. `publish-image.yml` calls it before it
builds.

`scripts/check.sh` is those gates locally, in groups (`rust sqlx test web site lint`).
Every CI job but the compose parse runs it, so local and CI cannot drift. The linters'
versions live only in `scripts/tools.sh`, which downloads them into `.tools/`. The git
hooks in `scripts/hooks/` are enabled by `scripts/dev-setup.sh` (`core.hooksPath`). Both
run `check.sh --at <rev>`, which checks that exact tree in a scratch worktree
(`.git/judgebot-check`, sharing `target/` and `.tools`, with its own `node_modules`). The
working tree cannot mask a failure. The `sqlx` group migrates a throwaway
`judgebot_check` database, never the development one.
- pre-commit checks the staged tree: the groups the staged paths touch, deletions
  included, plus `lint`.
- pre-push checks every pushed commit with every group, so Postgres must be up.

The hooks run on Claude's commits too. Don't bypass them with `--no-verify`. Fix the
failure instead.

Discord registers six commands: `/judge` (guild-only), `/card`, `/rule`, `/help`,
`/license` and `/forget`, which deletes the caller's ratings through
`CallStore::forget_user`.

- `/judge private:True` is `discord::Audience::Private`: ephemeral, no thread history
  read, never persisted, so no rating buttons and no prior call. `Data::answer` reaches the
  store only through `Audience::record`, which is `None` for it. The audience is carried
  in `Pending` through a "did you mean?" pick.
- `discord/cooldown.rs` is the per-user `/judge` window (`JUDGE_USER_LIMIT` per
  `JUDGE_USER_WINDOW_SECS`, default 6 per 600 s, `0` = off), charged after the concurrency
  permit so a "busy" is free. Picks are free.
- `/card` and `/rule` are model-free lookups, rendered by pure `render::card` / `rules`.
- An *Incorrect* rating's ephemeral reply names the operator and, when the source offer
  is a GitHub repository, links its `wrong_answer.yml` issue form (`render::report_url`).

**The source offer** (`judge_core::source`, AGPL §13). Every remote interface names the
repository the instance's source is in, the commit it was built from and the
licence/copyright:

- the web footer via `GET /api/about`, served beside `/api/health` whatever doors are off
- `/help` and `/license`
- the MCP instructions and `about` tool
- `judge-cli about`

`JUDGE_SOURCE_URL` overrides the repository (validated at load, `Config::source_offer`).
`crates/bot/build.rs` stamps the commit from `JUDGE_COMMIT` (+ `JUDGE_DIRTY`). Both are
Dockerfile build args. CI sets `JUDGE_COMMIT` to the sha, and a non-hash fails the build.
Without `JUDGE_COMMIT` it uses `git rev-parse HEAD` plus a dirty flag. An unstamped build says
"commit unknown" rather than guessing.

**The operator contact** (`judge_core::operator`) travels beside the source offer.
`JUDGE_OPERATOR_DISCORD` (a `DiscordUsername`) is required by the bot, and
`JUDGE_OPERATOR_EMAIL` (a `SupportEmail`) by `judge-api` whichever doors it opens.
"Required" is a type: `Data::new` takes a `DiscordOperator` and `App::new` a
`NetworkOperator`, made only by `Operator::for_discord` / `for_network`
(`Config::discord_operator` / `network_operator`). `judge-cli` and stdio `judge-mcp` hold
a plain `Operator` and need neither. A set-but-malformed value fails `Config` load
everywhere. `About` carries both as `operator_discord` / `operator_email`.

`GET /api/health` runs a `Probe` (the pool, 3 s timeout). The compose healthcheck invokes
bash by name, because `/bin/sh` is dash in the slim image and has no `/dev/tcp`.

## Reproducing a reported failure

When the user brings a bad or failed answer to troubleshoot, reach for the cheapest
harness that can reproduce it, in this order. The rule is about *how to drive the
pipeline*, not about how much to investigate.

1. **The `judge` skill, session mode** (`.claude/skills/judge/SKILL.md`, driven through
   `target/release/judge-cli begin|extract|rules|verdict`). Claude is the model, so there
   is no API spend and no model configuration to get right. The extraction, the rendered
   material, the citation validation and the rejection notices are the same code the bot
   runs. Almost every report is reproducible here: a rejected citation, a wrong card
   resolution, a thin or missing context, an unhelpful retry notice. Start here and stay
   here. It needs Postgres and nothing else, so check `docker compose ps` first. If the
   database is down, `docker compose up -d db` is enough. Neither `bot` nor `api` has to
   be running.
2. **MCP.** Use `judge-mcp` over `.mcp.json` locally when the tools are connected. Use
   `judge-api`'s `/mcp` (`--mcp` + `MCP_TOKEN`) when the user is away from this machine
   and the local database is not reachable. The operations are the same as the CLI's, so
   prefer whichever transport is available.
3. **A live instance** (`docker compose up -d bot api`, `judge-cli judge`, `judge-eval
   answer`). Use it only when the deployed surface is what's in question (Discord
   rendering, buttons, rate limiting, startup/config, the spend cap) or when step 1 has
   ruled the pipeline out. This spends money on model calls, so say what it will cost
   before starting it.

**The exception is a question about another model or provider.** When the report is
"Gemini/GPT/this endpoint answers badly through the bot", the model's own behaviour is the
thing under test and Claude-as-the-model reproduces nothing. Run the built-in pipeline against
that provider's `judge.toml`: `JUDGE_CONFIG=… judge-cli judge`, `judge-eval answer
--config <file>`, or the containers with that file mounted. This assumes the user has
that provider set up, so ask for the config rather than inventing one. The same applies
to anything provider-shaped: wire format, schema dialect, pricing, auth.

## Commands

Everything needs env from `.env` (`set -a; source .env; set +a`). Postgres runs in
Docker on **localhost:5432**. `DB_PORT` in `.env` moves the published port. The
containers always reach it at `db:5432`.

```sh
docker compose up -d                 # db (pgvector/pg16) + bot + api; all restart with Docker
docker compose up -d --build bot api # redeploy bot/api after code changes (one image, two entrypoints)
                                     # COMPOSE_PROFILES=tunnel also starts cloudflared (docs/DEPLOYMENT.md)
docker compose pull && docker compose up -d  # deploy host: pulls the CI-built GHCR image, never builds
scripts/backup-db.sh                 # weekly pg_dump -> Cloudflare R2; cron'd on the server
cargo build --workspace
cargo clippy --workspace --all-targets   # must be warning-free; lints deny unwrap/expect/indexing/panic,
                                         # and bare #[allow]: suppress with #[expect(lint, reason = "…")]
cargo test --workspace               # includes #[sqlx::test] suites that spin temp DBs off DATABASE_URL
cargo test -p judge-bot possessive   # run a single test by substring
scripts/check.sh [--staged | group..]   # the CI gates locally (the git hooks run this)
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

cargo run --release -p judge-api -- [--api] [--web] [--mcp]   # one flag per front door, all opt-in; no
                                                        # flags = POST /api/judge alone, on API_ADDR (:8787).
                                                        # --web needs a built web/dist and --mcp an MCP_TOKEN
                                                        # (both startup errors); a token with no --mcp only
                                                        # warns; GET /api/health is served whatever is off
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
                                                        # remote: judge-api --mcp serves /mcp (needs MCP_TOKEN)

cargo run -p judge-eval -- recall [--vectors]           # retrieval gate, no API keys (--vectors: the configured
                                                        # embedder, ~$0.001), exit≠0 below 90% retrieved or 75%
                                                        # shown under the synthesis budget
cargo run -p judge-eval -- answer --label L --limit 21 --max-usd 6.00   # full live gold run (~$2.50)
                                                        # --config judge.toml runs it on other providers
cargo run -p judge-eval -- rescore eval/runs/<run>.json # re-score a stored run, zero API cost
cargo run -p judge-eval -- show eval/runs/<run>.json    # bot vs gold answers side by side
```

## Architecture

Pipeline (`docs/ARCHITECTURE.md` §3 is kept current):

1. **Extraction + classification.** One low-effort LLM call with structured output.
2. **Card resolution.** A typed ladder: alias → possessive-stripped alias → exact →
   printed name → short-name-before-comma → alias-suffix → trigram fuzzy.
   - A `[[bracketed]]` span is `CardSpan::Exact` and takes only exact → printed name. A
     miss is offered only as `Ambiguous`: the alias / short-name hits under their own rung
     (so a duplicate is dropped), else fuzzy neighbours.
   - The resolved cards are stamped onto `Verdict<Validated>` (`cards()`) and every front
     door shows them.
   - Resolution **never guesses**. Ambiguity becomes `Resolution::Ambiguous` and a Discord
     "did you mean?" button row.
3. **Retrieval.** Three legs unioned in priority order: the primary category's CR
   subsections ranked by text relevance, tsvector BM25, pgvector cosine, then the
   secondary categories. Primary subsections sharing no word with the question go after
   the next two legs. The synthesis budget renders a prefix, so this order is what the
   model reads. Retrieval also adds rulings for all faces, glossary, nightmare-card notes
   and rated prior calls.
4. **Synthesis.** High-effort, with citation validation and one retry. At most one
   `lookup_rules` tool round, enforced by typestate.
5. **Persist** + Discord rating buttons.

Crate graph (`core` ← `llm` ← `anthropic` and `openai` ← `embed` ← `bot` and the bins):

- `core`: domain ADTs, ports, `judge()`, citation validation. It is pure.
- `llm`: the provider seam (`docs/PROVIDERS.md`).
  - Neutral `ChatRequest`/`ChatResponse`.
  - The open `Backend` trait providers implement, and the sealed `ChatModel` port the
    pipeline calls. Only `Metered<B>` implements `ChatModel`, so every send is behind the
    spend cap by type.
  - `SpendMeter` + `Price::{Free, Table, PerToken}`. A table price is re-read for the
    model the response names. An operator's `PerToken` settles at exactly its rate.
  - The shared HTTP retry loop, the `Synth` typestate and `classify`.
- `anthropic`: a `Backend`. Hand-written wire types we own, the `Endpoint` enum,
  neutral↔wire conversion. schemars → Anthropic's schema subset goes through a transform
  that must keep `additionalProperties:false` and rewrite `oneOf→anyOf`, applied at
  conversion time.
- `openai`: a `Backend` for chat completions. Its own wire types and the strict-schema
  transform (every property `required`, optionals `anyOf [T, null]`). String tool
  arguments are parsed by serde. `choices[0].message` is replayed verbatim.
- `embed`: Voyage + OpenAI-compatible `/embeddings`, each a `WithSpace`.
- `bot`: sqlx adapters, `config.rs` (the `judge.toml` loader), `extract.rs`/`synth.rs`
  over `judge-llm` only, prompts in `crates/bot/src/prompts/`, and the serenity/poise
  Discord layer with pure `render.rs`.
- `ingest` / `eval` / `api`: bins.
- `agent`: lib + `judge-cli` / `judge-mcp` bins. `api` mounts its MCP handler.

`judge_bot::build_deps(pool, Models, embedder)` is the composition root shared by the bot,
eval, the HTTP API and the agent's `judge` tool. `Models::{single, pair, priced}` take the
meter and bare backends and meter them themselves. Their fields are private, so there is
no uncapped model and no foreign meter. Every binary gets its `Models`/`Vectors` from
`config::Config::load()`. Its no-file branch (`from_vars`) is the zero-config setup:
Anthropic direct, one model for both stages, one `SpendMeter`.

`crates/bot/tests/anthropic_golden.rs` pins the four Anthropic request shapes
byte-for-byte against captured fixtures. `UPDATE_GOLDEN=1` re-captures them after an
intended prompt/schema change. Review the diff.

`api` (+ the SolidJS page in `web/`) is the anonymous front door. Which of its doors a
process opens is a launch option, not a consequence of starting it
(`crates/api/src/interfaces.rs`):

- `Interface` is exhaustive, held in a `NonEmpty` set so "serving nothing" is
  unrepresentable.
- `ApiConfig::check` refuses `--mcp` with no `MCP_TOKEN` and `--web` with no `index.html`
  before anything binds.
- The mirror case, a token with no `--mcp`, is a warning. Refusing there would take a
  working page down over a variable that exposes nothing.

Also true of `api`:

- `mcp::router` gates with `route_layer`, not `layer`. `layer` wraps a router's fallback
  too, and merging that into a router with no web fallback made the 401 the catch-all for
  every unrouted path.
- No ratings. "Did you mean?" is stateless, via `pins` → `pin_card` rewriting. Session
  history is keyed by a client UUID (`web:<uuid>` thread ids).
- Per-IP fixed-window rate limiting (`API_RATE_LIMIT`/`API_RATE_WINDOW_SECS`) sits ahead
  of the concurrency semaphore and the spend cap.

Key cross-file facts that aren't obvious from any one file:

- **Citations are typed and validated.** `Citation::{Rule, ScryfallRuling, OracleText,
  PriorCall}` each carry a verbatim `quote` checked as a substring of the source in
  `Context`.
  - `Quote` cannot be blank. A blank one fails to parse, so a placeholder citation is a
    `MalformedCitation` with the stub notice, not a bad citation. Its schema is plain
    `String`.
  - The check folds typographic punctuation (`judge_core::quote`): curly quotes, the dash
    block, non-breaking spaces. It maps one `char` to one `char`, never case or words.
    Models retype the CR's `’` as `'`, and that was the most common rejection.
  - What is stored is the *source's* span, not the model's string. A persisted quote
    stays byte-exact and the retirement pass's `citation_supported` stays a strict check.
  - A failed check, or an empty/citation-less verdict on an answerable source, becomes a
    retry with the rejection rendered into the prompt. The rejection is logged at INFO,
    so a second failure can be read against the first. The retry is a fresh conversation,
    so the rejected answer is quoted back as a blockquote (`RejectedAttempt`), except for
    a placeholder or over-long answer.
  - Only `Verdict<Validated>` can reach `CallStore::persist` or Discord rendering.
- **CR chunking is two-granularity.** `rules` rows exist at rule level (`702.19`). That
  body includes all lettered sub-rules + examples, gets an embedding and feeds retrieval.
  They also exist as leaf rows (`702.19b`, `parent_id` set), which are the citation
  targets. Scoring and `lookup_rules` treat leaf↔parent as covering each other.
- **Ratings shape retrieval, nothing else.** The `calls_rated` view is a Bayesian mean
  (prior 2.0, weight 3) with a judge-role override (`effective_score`). Prior calls below
  1.5 with ≥5 votes are excluded, as are retired calls. Prior calls are always rendered
  *after* CR material as examples.
- **A call is retired when its citations stop holding, not when the CR changes.**
  - `retire_unsupported` (`db/retire.rs`) is run by `ingest retire` and nightly inside
    `ingest refresh`. It re-runs `citation_supported` over every stored call against
    today's rules, rulings and Oracle text. It sets `calls.retired_at`/`retired_reason`
    both ways, so restored text brings a call back.
  - Each call also carries `context_ids.card_text` (an `oracle_fingerprint` per context
    card), so an erratum retires calls *about* the card even when they cited only the CR.
  - `cr_version` on a call is a record, not a gate.
  - Rulings are keyed by content (`ruling_key`, 16 hex chars), so a reindexed ruling is
    the same ruling. The key lives in core because the ingest writer and every reader
    must agree.
- **A renumbered rule keeps its calls.**
  - Inside the CR load transaction, `renumber_map` (`ingest/src/renumber.rs`) matches old
    and new rules by body with every rule id masked, because renumbering changes the
    cross-references too. It matches only where the masked body is unique on both sides.
  - It then keeps only entries that reproduce the new rule exactly when the old one is
    rewritten with the whole map. That is a fixpoint, so a redirected cross-reference is
    not mistaken for a renumbering.
  - `rewrite_call` then rewrites every call's `rule` citation ids, the ids inside
    `rule`/`prior_call` quotes and the answer text in one pass.
  - It never guesses: ambiguous or reworded rules are left to the retirement pass.
  - The CR loader and the retirement pass take the same advisory lock
    (`CALLS_REWRITE_LOCK`).
- **Agent sessions are the pipeline in pull mode, with the same validation.**
  - `judge_bot::session` (`Session`/`Stage` machine, `Sessions` over `PgSessionStore`,
    table `agent_sessions`) hands an outside agent the extraction prompt, then the
    synthesis prompt rendered from the same `Context`. It admits the agent's verdict only
    through `Verdict::validate`. The limits are the same: one `lookup_rules` round, one
    retry, same rejection notice.
  - `synth::system_prompt(Harness)` fills two tokens in `prompts/synth_system.md`
    (`{{LOOKUP_RULES}}`, `{{OUTPUT_FORMAT}}`). `Harness::Tool` is the tuned prompt the bot
    sends. `harness_tests` pin its SHA-256, so a template edit that changes it fails a
    test until the digest is updated on purpose.
  - Thread ids are `AgentThread` (`agent:<uuid>`, only mintable or parseable with the
    prefix), so a session can never read or write a Discord thread's history.
  - Inputs are bounded (`MAX_QUESTION_CHARS`, `MAX_EXTRACTION_ITEMS`, `MAX_LOOKUP_IDS`,
    `MAX_ANSWER_CHARS`).
  - Persisting is idempotent in the database (`calls.session_id` unique, `PersistCall`).
    A session-persisted call is thread history only. The prior-call leg skips
    `session_id IS NOT NULL` rows because nothing can rate them.
  - `Rejection` is adjacently tagged because it is stored.
  - The surface is `crates/agent`. `ops.rs` is the one list of operations, and `mcp.rs`
    and `bin/cli.rs` only transport. `.claude/skills/judge/SKILL.md` tells Claude Code how
    to drive it.
- **Vectors carry their space, and the database records the one it holds.**
  - Every embedder (`judge_embed::{VoyageEmbedder, OpenAiEmbedder}`) implements
    `WithSpace`: a `Space` (provider *kind* `voyage|openai`, model, dimensions).
  - The one-row table `embedding_space` names what the stored vectors are. Migration
    `20260904000001` seeds it `voyage/voyage-3.5/1024` for a DB that already held vectors.
    `Space::check` (pure, in `judge_embed::space`) is the only definition of "same space".
  - `ingest embed` writes the row on first use. It refuses on a mismatch or when the
    columns' actual `vector(N)` typmod differs (`db/space.rs` `column_width`). It never
    relabels vectors it did not write.
  - The adapters hold no bare `Embedder`. `PgRetriever`/`PgLibrary`/`PgCallStore` take
    `Arc<db::Vectors>` (`Config::vectors(pool)`, one per process), which embeds nothing
    until the stored space equals its own. A mismatch is an error-level log naming both
    spaces and dark vector legs, never a mixed column.
  - The row is re-read on every use, and once at startup so the verdict sits beside the
    config summary. A running bot picks up the first `ingest embed`, and a `reembed` under
    it darkens the legs instead of erroring or mixing.
  - Writers hold the space. `PgCallStore::persist` and every `ingest embed` batch take
    the shared side of `CALLS_REWRITE_LOCK` in their transaction and read the row under it
    (`hold_space` / `Vectors::hold`). `switch_space` takes the exclusive side, as the CR
    loader and the retirement pass do. A switch therefore waits for in-flight writes, and
    a write after it sees the new row.
  - `ingest reembed --yes` (`switch_space`) is the only thing that changes the row and the
    column width. It does so in one transaction, then runs the embed loop. It probes the
    embedder first (one short text), so a wrong key/URL/model/width fails before anything
    is cleared. The HNSW index definitions it recreates live beside it in `VECTOR_TABLES`,
    verbatim from the migrations.
  - `config::Dimensions` is `1..=2000` (HNSW's limit) at load.
  - A `[providers.X] kind = "openai"` table serves embeddings too. `[models.embed]` needs
    `dimensions` there, and `send_dimensions = false` is for servers that reject the field.
  - `ingest embed`/`refresh` load the same `Config`, so a deployment with a `judge.toml`
    mounts it into `refresh` as well.
- **Category taxonomy is data.** `data/categories.yaml` is the source of truth.
  `crates/core/build.rs` generates the `Category` enum from it, so taxonomy edits are
  recompiles and matches stay exhaustive. The extractor's schema makes the primary
  category a required field, so an empty classification is an API-level schema violation.
- **Gold eval set** (`eval/gold.yaml`): 21 adversarially verified questions with
  `expected_rule_ids` and per-question `equivalent_rule_ids` (alternate rule ids stating
  the same fact). The loader enforces that those keys are quoted and present in the
  expected list. Extend the set when adding capability. `rescore` re-grades old runs
  after gold edits. Rule ids written unquoted in YAML are rejected, because floats drop
  trailing zeros.
- **Discord layer:** interaction logic is kept pure and unit-tested (`render.rs`,
  `ids.rs` typed button custom-ids, `pending.rs` did-you-mean store, `question.rs` span
  pinning). Replies open with a non-pinging `<@user> asked:` header. Rule citations link
  to the Yawgatog CR mirror (anchor = `R` + id with dots stripped). Rulings and Oracle
  text link to Scryfall search-by-oracleid, because the `/card/<uuid>` route 404s.
- **Card symbols are pictures on both front doors.**
  - `discord/mana.rs` substitutes Discord application emoji (`{W}` → `<:mana_w:…>`).
  - `judge_core::symbol::emoji_name` is the one definition of the name. It lives in core
    because two programs (the bot and the `ingest emoji` uploader) must agree on it.
  - Text is carried as `mana::Rendered` segments rather than a `String`. A tag costs ~28
    of Discord's 2000/4096 characters and must never be cut in half, so plain text is the
    only cuttable segment.
  - An application with no emoji uploaded renders the literal `{W}`.
  - The web page does the same job with Scryfall's SVGs (`web/src/Symbols.tsx`).
- **Providers are configuration, not code.** `judge_bot::config` loads `judge.toml` into
  typed structs.
  - The file is `JUDGE_CONFIG`, else `./judge.toml` if present, else the default setup from
    `.env`: Anthropic direct, `claude-opus-5` both stages, Voyage if keyed.
  - `judge.example.toml` documents every knob with its default and must keep loading.
    `config::tests::the_example_file_loads_as_shipped` and
    `..._with_every_door_uncommented` pin that, so a renamed knob fails the gate.
  - The structs use `deny_unknown_fields` and nutype validators (`BaseUrl`, `Region`,
    `Project`, `WorkspaceId`, `Dimensions`). Secrets are named by `api_key_env` and read at
    load into a redacted `ApiKey`. A knob that would be ignored is an error naming both
    keys.
  - The chat backend `judge-anthropic` has `Endpoint::{Direct, Proxy,
    ClaudePlatformOnAws, Bedrock, Vertex}`, one `Endpoint` per provider table, shared by
    the stages naming it.
    - The cloud doors sit behind judge-anthropic's `aws`/`gcp` Cargo features. These are
      default on, forwarded from judge-bot's own features and named in the Dockerfile. A
      lean build cannot name the doors, and the loader says "not built".
    - Their credentials come from the platform chains, never `judge.toml`: SigV4 via
      aws-config (service `aws-external-anthropic` with the `anthropic-workspace-id`
      header, or `bedrock-mantle`) and ADC via gcp_auth. They are resolved lazily and
      probed once at startup by `Config::probe_auth`, so an empty chain fails there, not
      per question.
    - Proxy/Vertex mask the `fallbacks` beta. Bedrock also masks `output_config.format`,
      tool `strict` and every `anthropic-beta`. Verified against the live docs 2026-09-02.
  - The chat backend `judge-openai` is chat completions with `Dialect` knobs:
    `structured_output`, `strict_tools`, `reasoning_effort`, `max_tokens_param`,
    `cache_hints`. Its embeddings side has `send_dimensions`.
  - A model on an `openai` provider must be priced (`[models.X.pricing]`, cache prices
    defaulting high from `input`) or the provider `pricing = "free"`. The built-in table
    (`judge_llm::PRICES`) errs high for unknown Anthropic models only.
  - When a backend cannot enforce the output schema, the adapters append it to the *user
    turn* (`judge_llm::schema_block`), so the pinned system prompt digest and the
    Anthropic golden fixtures never change.
  - Every binary logs `Config::summary()` at startup. `judge-cli config` prints the
    redacted resolution. `eval answer --config` records `provider/model` per stage in the
    run file.

## Environment

`.env` is gitignored, with a template in `.env.example`. It holds:

- `DATABASE_URL` (port 5432, or `DB_PORT`).
- `ANTHROPIC_API_KEY`.
- `VOYAGE_API_KEY`. Blank turns the vector leg off, and the bot still works. A
  `judge.toml` `[models.embed]` overrides it, including OpenAI-compatible embeddings.
- `JUDGE_CONFIG`: optional path to a `judge.toml` (see above). It is a *host* path.
  - `cargo run` reads it as is.
  - `docker-compose.yml` bind-mounts it into `bot`/`api`/`refresh` at
    `/etc/judgebot/judge.toml` and points their `JUDGE_CONFIG` there
    (`${JUDGE_CONFIG:+…}`). Blank mounts the tracked example, which nothing reads.
  - So a `./judge.toml` in the repo root is read by `cargo run` but invisible to the
    containers until `JUDGE_CONFIG` names it.
  - Editing the mounted file's content is not a change `up -d` recreates for, so
    `docker compose restart bot api`.
  - The `api_key_env` of every provider a stage names lives in `.env` too. A table no
    stage names is parsed, but its key is never read.
  - The cloud doors' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS` live in `.env` as well,
    never in `.env.deploy`, which `bot`/`api` do not read.
- `DISCORD_TOKEN`.
- `GUILD_ID` (instant command registration).
- `JUDGE_ROLE` (default "Judge").
- `JUDGE_MAX_USD`, `JUDGE_CONCURRENCY`.
- `JUDGE_AUTO_MIGRATE` (default true). When true, bot and api apply pending migrations
  at startup. `judge-ingest migrate` is the explicit form.

For the HTTP API it also holds:

- `API_ADDR` (default `0.0.0.0:8787`).
- `WEB_DIST` (read only under `--web`).
- `API_INTERFACES`: the flags the `api` container passes, default `--api --web`. It is
  read by `docker-compose.yml`, not by the binary.
- `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS`.
- `API_CLIENT_IP` (`peer` or `cloudflare`, see below).
- `MCP_TOKEN`: the credential for `/mcp`, which also needs the `--mcp` interface. ≥24
  chars, bearer-checked before the protocol.
- `MCP_ALLOWED_HOSTS`: the `Host` values the MCP transport accepts, meaning the public
  hostname behind the tunnel.
- `MCP_JUDGE_LIMIT`/`MCP_JUDGE_WINDOW_SECS`: `judge` runs per window through `/mcp`. This
  is the blast radius of a leaked token.

The bot/api containers override `DATABASE_URL` to `db:5432` inside the compose network.
The image builds the web page and sets `WEB_DIST=/srv/web`. The `api` service's `command`
is `${API_INTERFACES:---api --web}`, so the page stays on for a compose deployment while
`judge-api` on its own serves no page.

Deployment is self-hosted behind a Cloudflare Tunnel. `docs/DEPLOYMENT.md` is the
runbook. `db` and `api` publish on `127.0.0.1` only. Public traffic reaches `api:8787`
over the compose network from the `cloudflared` service, which the `tunnel` compose
profile starts (`COMPOSE_PROFILES=tunnel` in `.env`). Deploy credentials live in
`.env.deploy` (`TUNNEL_TOKEN`, `R2_*`). Only `cloudflared` and `scripts/backup-db.sh` read
it, never the internet-facing `bot`/`api`. Weekly `scripts/backup-db.sh` dumps to R2 and
has `list`/`fetch` subcommands for the restore drill. Restoring is far cheaper than
re-ingesting, which re-pays the embedder per row, so take a backup before
`ingest reembed --yes` (runbook in `docs/DEPLOYMENT.md` §7).

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` (socket address) or `cloudflare` (`CF-Connecting-IP`). `client_ip` never reads
`X-Forwarded-For`, because Cloudflare *appends* to a caller-supplied header instead of
replacing it. Its first hop is attacker-chosen, which would hand every request a fresh
allowance against a paid endpoint. `cloudflare` is only sound when nothing can reach the
origin except Cloudflare.

**Data refresh is a nightly cron on the deploy host**, not a service.
`scripts/refresh-data.sh` runs the `refresh` compose service (profile `refresh`, third
entrypoint `judge-ingest` in the same image). `docker compose run` enables the profile
itself, so `up -d` never starts it. CR release detection scrapes Wizards' rules page for
the `MagicCompRules <date>.txt` link and compares the date to the stored `cr_version`.
The CR loader nulls embeddings only for rules whose text changed, so a new CR costs the
embedder a few hundred rules. `aliases` and `notes` are not part of refresh. They are repo
data, loaded when they change.
