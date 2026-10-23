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
  spend-capped `judge_llm::Metered` (`JUDGE_MAX_USD`, default $5), and every embedding
  through `judge_embed::MeteredEmbedder` on the same meter. Each run's own cost is a
  `judge_core::Tally` passed to every port that spends (`ChatModel::complete` takes a
  `Bill`), recorded in `judge_runs` by `judge_bot::run::run` (D33). `JUDGE_BUDGET_PERIOD`
  (`process` | `day` | `month`) says what the cap covers (D19): `judge_bot::budget` keeps
  the `spend_days` ledger every 10 s and sets the meter's *adjustment*, so every process
  shares one period total that survives restarts. A `judgebot` process has one meter
  whichever roles it runs. `judgebot ingest` and a `--jobs`-only process write the ledger
  too. The meter itself stays storage-free.
  `JUDGE_SPEND_WARN_AT` (percentages of the cap, unset by default) warns before it, once
  per threshold per period, claimed in `spend_warnings` so one process sends each.
  `JUDGE_ALERT_WEBHOOK` is told when the cap trips or a warning is due, when the scheduled refresh fails (once
  per streak), recovers or hits its embed ceiling, when a script fails
  (`scripts/alert.sh`), and when the Discord role finds `DISCORD_INTERACTIONS` and the
  application's Interactions Endpoint URL disagree, or refuses a signed interaction for
  its timestamp (once per streak; a wrong host clock refuses them all). Alerts are written
  in `JUDGE_LOCALE` (`judge_bot::alert::catalog`). `judge-cli stats` reads the ledger and the period and cap the processes record in `budget_setting`. The cap *reserves*
  worst-case cost before sending, so caps under ~$0.36 refuse synthesis outright. Develop
  against wiremock, not the live API. A full 24-question gold run costs ~$1.85.
- Claude manages commits in this repo: commit completed, verified steps without asking.
  Never commit `.env`, `.cache/`, or `eval/runs/`. A run worth publishing is copied to
  `eval/published/` on purpose, and the README's Results table and the site's Sample
  answers page (`start-here/sample-answers.md`, verbatim from that file) are updated with
  it.
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
configuration reference, model choice (`judge.toml`), the config editor, Discord commands, the
web app, the HTTP API, agents, command reference, data files, attribution, schema. The README carries
short versions of the model, web and API material and links to those pages.

`npm --prefix site run build` runs the sync first. `publish-docs.yml` deploys `site/dist`
to GitHub Pages on pushes touching the sources.

`docs/` holds five files: `ARCHITECTURE.md` (what exists), `DECISIONS.md` (why, D1–D42),
`PROVIDERS.md` (the model-provider reference), `DEPLOYMENT.md`, `EXPLAINER.md`. Retired
proposals live in git history only.

- **One judgebot per community** (D16). The docs teach an operator to create their own
  Discord application. There is no tenancy layer and none is planned.
- **No instance is ever named.** There is no bot to invite, no "try it" link and no
  running deployment referenced anywhere in the repository or the site. That includes a
  hostname and "the maintainer's instance". The reader's own instance is the only one
  that exists.
- Section headings are short noun phrases, a few words, never a clause or a
  "X, and what Y" pair ("Validation", not "What it checks, and what it leaves to
  startup"). Detail belongs in the first sentence under the heading. A `DECISIONS.md`
  title names the decision in the same spirit. Check that no link anchors on a heading
  before renaming it (`#d16-one-judgebot-per-community` is one).
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
Yawgatog). Starlight renders the footer on splash pages too, so it is on every page. Its
text lives in `site/src/i18n.ts`. The web app's footer (`web/src/App.tsx`, text in
`web/src/messages.ts`) carries the same text; change them together.

**The icon** is 32x32 pixel art and must stay pixelated, so it is a PNG, never an SVG or a
smoothed resize. `assets/icon.png` is the 512px original (the README, a Discord app's
avatar). Copies: `site/src/assets/icon.png` (header logo) and `site/public/icon.png`
(`og:image`). `favicon.png` (the native 32px grid) and `apple-touch-icon.png` (192px,
nearest-neighbour, on `#1f2933`) sit in both `site/public/` and `web/public/`
(`scripts/check.sh lint` fails when copies of one size differ or a size is wrong, but
does not check how a copy was scaled). Regenerate every copy from the original when it
changes, and scale only by whole multiples of 32.
Wherever it is displayed scaled, the CSS sets `image-rendering: pixelated`.

Check site changes in a browser with the `playwright-cli` skill. Run it against
`npm --prefix site run dev` (port 4321) or a preview of the build (what Pages serves at
`https://mtg-judgebot.rpeters.dev`). Both use base `/`. Take the page at 1920, 1440, 820 and 390
wide, on a splash page *and* a docs page. Read positions out of
`getBoundingClientRect()` rather than by eye.

CI is `.github/workflows/ci.yml`: fmt, clippy twice (every target, the dev-only stub
locale unified in; then the libraries and binaries alone, the features that ship), `.sqlx` freshness, tests on a pgvector
container started by a step (after a Docker Hub login, when the secrets exist), web and site builds, compose parse. It also runs Biome over `web/` and `site/`
(`biome.json` at the root, `.astro` files left to `astro check`), `astro check`, and
lychee over the built site's internal links. A `lint` job runs `cargo deny check`
(`deny.toml`), `cargo machete`, `taplo fmt --check` (`.taplo.toml`, which leaves out
`judge.example.toml`'s hand-aligned comments), `typos` (`_typos.toml`), shellcheck,
actionlint, hadolint (`.hadolint.yaml`), `check_icons` (the icon's copies are
byte-identical and the right size) and `check_pg_major` (the `Dockerfile`'s `PG_MAJOR`
equals the compose `db` image's major, so the backup's `pg_dump` can dump it),
`check_stub_locale` (the workspace's feature graph without dev-dependencies,
`cargo tree -e features,no-dev -i judge-core`, has no `stub-locale`, and neither
`Dockerfile` nor a workflow names it or `--all-features`) and
`scripts/release-version.sh` (web/ and site/'s `package.json` versions equal the crates'
one `[workspace.package]` version; `publish-image.yml` runs it with the release tag,
which must match, and its `## [X.Y.Z]` changelog heading). Every tool's config records why each
ignore is there. `publish-image.yml` calls it before it
builds.

`scripts/check.sh` is those gates locally, in groups (`rust sqlx test web site lint`).
Every CI job but the compose parse runs it, so local and CI cannot drift. The linters'
versions live only in `scripts/tools.sh`, which downloads them into `.tools/`. The git
hooks in `scripts/hooks/` are enabled by `scripts/dev-setup.sh` (`core.hooksPath`). Both
run `check.sh --at <rev>`, which checks that exact tree in a scratch worktree
(`.git/judgebot-check`, sharing `target/` and `.tools`, with its own `node_modules`). The
working tree cannot mask a failure. The `sqlx` group migrates a throwaway
`judgebot_check` database, never the development one. The `sqlx` and `test` groups take
turns per Postgres cluster (a lock file keyed on `DATABASE_URL`'s host and port, waited
on for up to 15 min): `#[sqlx::test]` names each throwaway database after the test's path
alone, so two suites on one cluster drop each other's databases mid-run.
- pre-commit checks the staged tree: the groups the staged paths touch, deletions
  included, plus `lint`.
- pre-push checks the tip of each pushed ref against the remote's sha (`--since`). It
  runs only what pre-commit leaves out: `sqlx` and `test` when Rust, `eval/` or
  `docker-compose.yml` changed (so Postgres must be up), and `lint`. `rust`, `web`
  and `site` already ran on each commit's staged tree. A change to the gates
  (`check.sh`, `tools.sh`, workflows), a new branch or an unfetched remote sha gets
  every group.

The hooks run on Claude's commits too. Don't bypass them with `--no-verify`. Fix the
failure instead.

Discord registers six commands: `/judge` (guild-only), `/card`, `/rule`, `/help`,
`/license` and `/forget` (`discord::command`: `definitions` is the registration, pinned
byte-for-byte by `tests/fixtures/discord/commands.json`, and `Command::parse` the typed
invocation; no poise, whose dispatch needs a live gateway connection; replies go
through `discord::reply::Reply`, initial response then follow-ups, over REST). `/forget`
deletes the caller's ratings with their reasons and feedback-queue items (`feedback` in
`Forgotten`) and removes their id and username from their calls and
`failed_calls` rows (a private call's text too) through `CallStore::forget_user`
(`Forgotten`).

- **Every stored call names its origin** (`judge_core::origin::Origin`, D31): the
  interface (`calls.interface`), the Discord asker (`asker_id`, `asker_name`, the unique
  username snapshotted; `DiscordUsername` or NULL) and `Visibility`. `CallStore::persist`
  and `record_failure` take it; `Origin::history_scope` decides which calls of a thread
  a question reads (`HistoryScope`). Private calls are never prior calls (`AND NOT
  c.private`), never embedded, and never a public question's history. The agent
  toolbox's `Transport` (Cli | Mcp) gives its `judge` calls their interface; sessions
  store `private` in `agent_sessions`.
- `/judge private:True` is `discord::Audience::Private`: ephemeral, no thread history
  read, persisted private with rating buttons. A failed call goes to `failed_calls`
  (D27; capped, 30 days / 500 rows; `judge-cli failures`). The audience is carried in
  the "did you mean?" button's custom-id through a pick.
- A "did you mean?" pick keeps no state in memory. The question is read back from the
  bot's own message (`render::PickPrompt`: `<@asker> asked: ` + the question verbatim +
  a blank line + a body with no blank line: lead line, numbered choices, notes; `parse`
  re-renders and compares). The asker, audience, the span's byte range, a digest of the
  prompt's content (`pick::Digest`) and the card's oracle id are in the custom-id
  (`ids::Pick`, `card:…`; the old `pick:<uuid>:<n>` parses to `LegacyPick` = expired).
  A click is refused unless the message's content matches the digest, its EPHEMERAL
  flag agrees with the audience (`pick::audience`; flags missing = private) and the card
  is on the prompt's list. The prompt's age comes from the message's last edit or its
  snowflake (`discord/pick.rs`). `pick_claims` (keyed by message id + that time + the
  digest, no text) makes each prompt single-use, claimed after the concurrency permit
  and given back (`release_pick`) if the acknowledgement fails. `/judge`'s question has `max_length = 1300`
  (`QUESTION_MAX_CHARS`, const-asserted under `render::PICK_QUESTION_LIMIT`).
- `discord/cooldown.rs` is the per-user `/judge` window (`JUDGE_USER_LIMIT` per
  `JUDGE_USER_WINDOW_SECS`, default 6 per 600 s, `0` = off), charged after the concurrency
  permit so a "busy" is free. Picks are free.
- `/card` and `/rule` are model-free lookups, rendered by pure `render::card` / `rules`.
- An *Incorrect* rating's ephemeral reply names the operator and, when the source offer
  is a GitHub repository, links its `wrong_answer.yml` issue form (`render::report_url`).
- **Ratings with reasons** (D38): a rating click reads `CallStore::earlier_rating` within
  `EARLIER_BUDGET` (1.5 s from the interaction's arrival, `Ack::deadline`; no defer), then `judge_core::feedback::next_step`: the same Correct
  is already recorded; another earlier rating gets Replace/Keep (`rate-replace:` /
  `rate-keep:`, which reads the rating back and says what is on record); Correct is recorded; Partial/Incorrect open the form
  (`discord::feedback`). Over HTTP the form is one modal written as JSON (Label + string
  select + text inputs), its values read from the raw body (`feedback::submitted`),
  because serenity 0.12.5 parses a Label as an empty action row and the gateway gives
  no raw payload. Over the gateway a `rate-cat:` menu comes first, then a rows modal,
  the category in `ids::FormId`. Every initial response goes through `respond`,
  `respond_modal` or `respond_raw` (`ack.sent()`).
- **Interactions over HTTP** (`DISCORD_INTERACTIONS=http`, `discord::Delivery`, D30):
  no gateway, no lease. `serve` reads the application (`GET /applications/@me`,
  retried) for its id and `verify_key`, registers, starts `symbols::watch` and opens
  `endpoint::Interactions`, served at `endpoint::PATH` by `judge_api::discord` (on the
  network roles' listener, or `Listening::interactions_only` on `API_ADDR` when
  `roles::Discord::listen` is set: `--discord` alone). In http mode that listener is
  `http::reserve`d (bound, refusing dials) and `listen`s only once the endpoint opens,
  or after `network::LISTEN_WAIT` (30 s). `receive`: wait up to
  `OPEN_WAIT` for it to open (else 503), verify (401), PING inline, else dispatch on a
  `Work`-holding task and answer 202 once `reply::Ack` hears the initial response (REST
  callback) or at `ACK_WAIT` from arrival. Every initial `create_response` must call
  `ack.sent()`. `/api/health` fails until it is open (`discord::Open`), and the compose
  healthcheck probes `--discord` alone when the variable is `http`. A mismatch with the
  application's Interactions Endpoint URL (`endpoint::mismatch`) is an ERROR plus the
  alert webhook, never a refusal: Discord saves the URL only once the endpoint answers.
- **One gateway holder** (`discord/gateway.rs`, `DISCORD_INTERACTIONS=gateway`, the default). `discord::serve` runs `run` inside
  `gateway::hold`, in a loop: take a `GatewayLease` or stand by for it (unbounded, INFO
  `standing by: …` naming the holder, reconnecting with backoff if the waiting
  connection drops; the process's HTTP roles serve meanwhile), wait `Timing::grace`,
  check, connect. The holder checks every `CHECK_INTERVAL` (5 s, `check_timeout` 4 s);
  a lost lease sends `Stop` (`run` repeats `ShardManager::shutdown_all` until `start`
  returns, since it is a no-op before a shard registers) and waits `shutdown_limit`
  (1 s). Closed in time: stand by again. Not: return an error so the process exits.
  `grace` (15 s) > `worst_disconnect` (10 s) is a `const` assertion. The gateway is an
  `FnMut(Stop)`, called per connection over one `Arc<Data>`; `symbols::watch` is
  spawned on the connection's first `Ready` and aborted with it (`AbortOnDrop`). Commands
  register once per role (`Data::registered`), bounded by `REGISTER_TIMEOUT`, and a
  failure is a WARN. A loss within
  `Timing::healthy_hold` (30 min) of the last connection pauses before standing by
  (`reconnect_pause`: 30 s doubling to 10 min, ERROR), for Discord's 1000 IDENTIFYs a
  day per token. `Timing::every` returns `None` for a zero interval. A holder in a paused VM that loses its session can overlap until its
  next check. The old `bot` image takes no lease, hence `--remove-orphans` on the
  upgrade. Tests inject the gateway as a closure; never open a real gateway in one.
- **Graceful shutdown** (`judge_bot::shutdown`, D29). SIGTERM or Ctrl-C (`main::listen`,
  installed first; a signal before the roles start, or a second one, exits at once) requests the process's one `Shutdown`, held by
  `Serving`. `hold` then stops standing by, or closes the gateway and releases the lease
  once it has closed (`leave`; one slow to close keeps the lease until it has, and
  at the limit lets go: `FLUSH_LIMIT` < grace is a `const` assertion). A close within
  `shutdown_limit` releases with `GatewayLease::release_closed`, which writes
  `gateway_handover.released_at`; every holder's `take_over` clears it before connecting
  and skips the grace (`Handover::Clean`) only for a release under
  `Timing::handover_window` (5 s, `const` < grace) old. `discord::Handler` holds a `Work` guard for each
  interaction serenity dispatches, and `serve` returns after `Shutdown::drained`. `serve_on` is
  axum's graceful shutdown, bounded. All of it is within `DRAIN_LIMIT` (120 s) of the
  request. `main::run` waits for both roles on a requested stop (otherwise the first to
  stop still ends the process), waits for the scheduler to stop its run and end
  (`Scheduler::stopped`, within the drain deadline), then `Serving::flush` writes the
  ledger, exit 0.
  Compose's `stop_grace_period` is `DRAIN_LIMIT + EXIT_MARGIN` (a test).

**The source offer** (`judge_core::source`, AGPL §13). Every remote interface names the
repository the instance's source is in, the commit it was built from and the
licence/copyright:

- the web footer via `GET /api/about`, served beside `/api/health` by the network roles whatever
  interfaces are off (not by `--discord` alone over HTTP, whose listener serves the
  endpoint and `/api/health` only)
- `/help` and `/license`
- the MCP instructions and `about` tool
- `judge-cli about`

`JUDGE_SOURCE_URL` overrides the repository (validated at load, `Config::source_offer`).
`crates/bot/build.rs` stamps the commit from `JUDGE_COMMIT` (+ `JUDGE_DIRTY`). Both are
Dockerfile build args. CI sets `JUDGE_COMMIT` to the sha, and a non-hash fails the build.
Without `JUDGE_COMMIT` it uses `git rev-parse HEAD` plus a dirty flag. An unstamped build says
"commit unknown" rather than guessing.

`About` also carries `freshness` (`judge_core::Freshness`: the CR release loaded, the age
of the last successful refresh, whether the latest failed), read from `refresh_runs` by
`ingest::runs::freshness` within 2 s. Unreadable is `null` plus a WARN, never an error;
a missing run table degrades to the CR version alone. `FreshnessReader` caches it for a
minute, single-flight.
`SourceOffer::about` takes it as an argument, so every caller decides. `/api/health`
ignores it.

**The operator contact** (`judge_core::operator`) travels beside the source offer.
`JUDGE_OPERATOR_DISCORD` (a `DiscordUsername`) is required by `--discord`, and
`JUDGE_OPERATOR_EMAIL` (a `SupportEmail`) by the network roles (`--api`, `--web`, `--mcp`).
"Required" is a type: `Data::new` takes a `DiscordOperator` and `App::new` a
`NetworkOperator`, made only by `Operator::for_discord` / `for_network`
(`Config::discord_operator` / `network_operator`). `judge-cli` and stdio `judge-mcp` hold
a plain `Operator` and need neither. A set-but-malformed value fails `Config` load
everywhere. `About` carries both as `operator_discord` / `operator_email`.

**Release check.** `migrate::run` records the binary's release and commit in
`instance_release`. A migrating startup decides under the migration lock (`apply`'s
`Ahead::Skip` branch; `check_startup` when `JUDGE_AUTO_MIGRATE=false`) whether unknown
migrations are a rollback it can run (`rollback_refusal`, `Error::Rollback`): within a
major yes, across one, between builds of a new major's pre-release (`X.0.0-*`,
`Release::is_major_prerelease`) or with nothing recorded, no. A standalone toolbox
(`judge_agent::Client::Standalone`: `judge-cli`, stdio `judge-mcp`) compares its release
with it the same way (`judge_core::release::compatibility` over a `Ledger`: migrations it
lacks, migrations it does not know) through `release::ReleaseCheck`, cached for a minute,
failing closed. The ports live in `gated::Db`, reachable only as `Checked` from
`Db::check`, so an op cannot skip it (`about`'s freshness is the one unchecked read).
Same major, newer instance: a note; otherwise a stop. `main` carries `X.Y.Z-dev` between
releases. Within a major, migrations only add (CONTRIBUTING); dropping, renaming and
rewriting rows wait for a major, made during its `X.0.0-dev` cycle.

**Maintenance during a major upgrade** (D32). `migrate::upgrade` is `InPlace` (migrate,
then open) or `Major` (another major, a new major's pre-release, or no recorded
release with migrations pending). `Major`: `main` starts the roles with
`maintenance::Availability::upgrading()`, then `SchemaHold::exclusive` (every process
holds `SCHEMA_LOCK` shared for life) → `at_startup` → `downgrade` → `set_open`, and only
then the scheduler. Closed: Discord commands but `/help`/`/license` get
the catalog's `maintenance`, `/judge` a `RetryPrompt` + `retry:` button (`ids::Retry`, claimed
in `pick_claims`, charged to the cooldown on retry); `/api/judge` 503 `maintenance` with
`Retry-After`; `/mcp` ops fail through `gated::Guard::Instance`; `/api/health` fails
through the `Maintenance` probe.

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
   database is down, `docker compose up -d db` is enough. `judgebot` does not have to
   be running.
2. **MCP.** Use `judge-mcp` over `.mcp.json` locally when the tools are connected. Use
   `judgebot --mcp`'s `/mcp` (`MCP_TOKEN`) when the user is away from this machine
   and the local database is not reachable. The operations are the same as the CLI's, so
   prefer whichever transport is available.
3. **A live instance** (`docker compose up -d judgebot`, `judge-cli judge`, `judge-eval
   answer`). Use it only when the deployed surface is what's in question (Discord
   rendering, buttons, rate limiting, startup/config, the spend cap) or when step 1 has
   ruled the pipeline out. This spends money on model calls, so say what it will cost
   before starting it. The compose service's default roles include `--jobs`, which
   refreshes the database it points at for real: against the development database, set
   `JUDGE_REFRESH_HOURS=0` (or a `JUDGE_ROLES` without `--jobs`) in `.env` first.

**The exception is a question about another model or provider.** When the report is
"Gemini/GPT/this endpoint answers badly through the bot", the model's own behaviour is the
thing under test and Claude-as-the-model reproduces nothing. Run the built-in pipeline against
that provider's `judge.toml`: `JUDGE_CONFIG=… judge-cli judge`, `judge-eval answer
--config <file>`, or the containers with that file mounted. This assumes the user has
that provider set up, so ask for the config rather than inventing one. The same applies
to anything provider-shaped: wire format, schema dialect, pricing, auth.

## Commands

Binary names: `judgebot`, `judge-eval`, `judge-cli`, `judge-mcp` and `judge-config` are
what `target/release/` holds. `judgebot` is the one long-running binary: its roles
(`--discord --api --web --mcp --admin --jobs`, else `JUDGE_ROLES`) are launch options, and
`judgebot ingest <cmd>` is the data command line (`crates/judgebot`). `judgebot` is the
image's entrypoint. Locally run `cargo run -p judgebot -- <roles>`.

Everything needs env from `.env` (`set -a; source .env; set +a`). Postgres runs in
Docker on **localhost:5432**. `DB_PORT` in `.env` moves the published port. The
containers always reach it at `db:5432`.

```sh
docker compose up -d                 # db (pgvector/pg16) + judgebot (JUDGE_ROLES, default
                                     # --discord --api --web --jobs) + proxy (127.0.0.1:8787);
                                     # all restart with Docker
docker compose up -d --build judgebot  # redeploy after code changes
                                     # COMPOSE_PROFILES=public also starts edge (HTTPS on 80/443 for
                                     # JUDGE_DOMAIN), `tunnel` cloudflared, `backup` the backup
                                     # service (docs/DEPLOYMENT.md §3)
scripts/deploy.sh [--build]          # blue-green deploy (D29): pull (or build), start a second
                                     # judgebot beside the running one, wait for healthy, stop
                                     # the old one (it drains); an unhealthy new one is removed.
                                     # A major (`judgebot ingest upgrade` says): backup if `BACKUP_S3_BUCKET` is
                                     # set, new beside old in maintenance, old stopped once the
                                     # new one listens, then wait for the migration (D32).
                                     # Every deploy first refuses a set R2_* or a BACKUP_S3_*
                                     # set twice in .env.deploy, runs `judgebot backup check`
                                     # with the new image under the backup profile, and ends
                                     # with `up -d --no-deps backup`
docker compose pull && docker compose up -d --remove-orphans  # stop-then-start deploy: pulls the
                                     # CI-built GHCR image, never builds; --remove-orphans drops the
                                     # pre-judgebot `bot`/`api` containers (D25)
docker compose run --rm backup run  # one backup now (judgebot backup; the service's command by hand)
docker compose run --rm --no-deps backup check         # the settings and pg_dump, nothing sent
docker compose run --rm --no-deps backup list          # --no-deps: works with db down
docker compose run --rm --no-deps -T backup fetch <name> > <name>   # -T: bytes to stdout, no tty
scripts/backup-db.sh [list|fetch NAME]   # the same backup from a host cron, without the profile
cargo build --workspace
cargo clippy --workspace --all-targets   # must be warning-free; lints deny unwrap/expect/indexing/panic,
                                         # and bare #[allow]: suppress with #[expect(lint, reason = "…")]
cargo test --workspace               # includes #[sqlx::test] suites that spin temp DBs off DATABASE_URL
cargo test -p judge-bot possessive   # run a single test by substring (judge-bot is the library)
scripts/check.sh test                # the suite, waiting for any other sqlx/test run on the cluster:
                                     # use it, not bare cargo test, when another session or a hook
                                     # may be testing (sqlx test DB names collide across processes)
scripts/check.sh [--staged | group..]   # the CI gates locally (the git hooks run this)
SQLX_OFFLINE=true cargo build --workspace   # must pass; regenerate .sqlx after SQL changes:
cargo sqlx prepare --workspace -- --all-targets

cargo run -r -p judgebot -- ingest migrate              # apply pending migrations explicitly (judge_bot::MIGRATOR);
                                                        # judgebot does this at startup unless JUDGE_AUTO_MIGRATE=false
~/.cargo/bin/sqlx migrate run --source crates/bot/migrations   # the same thing with sqlx-cli
cargo run -r -p judgebot -- ingest init                 # the whole first load: migrate, cards, rules latest,
                                                        # aliases, notes, retire, embed, emoji; fail-fast,
                                                        # idempotent; recorded as a manual refresh run
                                                        # (in the image: docker compose run --rm refresh init)
cargo run -r -p judgebot -- ingest cards                # Scryfall bulk sync (cached in .cache/)
cargo run -r -p judgebot -- ingest rules <url|path>     # CR parse from a given file or URL
cargo run -r -p judgebot -- ingest aliases [yaml]       # no file = the data/aliases.yaml built into the binary
cargo run -r -p judgebot -- ingest notes [yaml]         # likewise data/notes.yaml (include_str!, so the image
                                                        # needs no data/ directory); both record the source
                                                        # (builtin | file) in curated_lists
cargo run -r -p judgebot -- ingest embed                # only rows with NULL embedding; the configured
                                                        # embedder ([models.embed] or VOYAGE_API_KEY); refuses
                                                        # if embedding_space or the columns' width differ
cargo run -r -p judgebot -- ingest reembed [--yes] [--clear]  # make the DB hold the configured embedder's
                                                        # space: when it holds another (row or column width),
                                                        # retype vector columns, rebuild HNSW, NULL every
                                                        # vector, rewrite embedding_space, then embed all; when
                                                        # it already holds it, only fill empty rows (idempotent;
                                                        # resume an interrupted refill with it). --clear clears
                                                        # and re-pays every row in the same space. Without --yes:
                                                        # prints rows, the cost at the configured price and
                                                        # what the spend cap has left, exit≠0, changes nothing.
                                                        # Billed to JUDGE_MAX_USD: raise it for the run if
                                                        # the dry run says the re-embed may not fit.
                                                        # `docker compose restart judgebot` after a switch
                                                        # (`up -d` sees no change: the file is a mount).
cargo run -r -p judgebot -- ingest emoji                # Scryfall card symbols -> the bot's Discord
                                                        # application emoji; idempotent, no DB needed
cargo run -r -p judgebot -- ingest rules latest         # the CR linked from Wizards' rules page, only if
                                                        # its version differs from max(rules.cr_version)
cargo run -r -p judgebot -- ingest retire               # retire/restore calls by whether their citations
                                                        # (and their context cards' Oracle text) still hold
cargo run -r -p judgebot -- ingest prune                # delete calls older than JUDGE_CALL_RETENTION_DAYS
                                                        # (with their ratings; judge_runs stay, unlinked)
cargo run -r -p judgebot -- ingest refresh              # cards + rules latest + lists + prune + sweep + retire + embed + emoji; every
                                                        # step runs even if one fails, exit≠0 if any did;
                                                        # recorded in refresh_runs
scripts/refresh-data.sh              # a refresh now (or an operator's own cron with JUDGE_REFRESH_HOURS=0):
                                     # `docker compose run --rm refresh`; judgebot --jobs runs it on a schedule

cargo run -r -p judgebot -- --api --web                 # roles: --discord --api --web --mcp --admin --jobs, at least
                                                        # one (none: JUDGE_ROLES). Requirements are checked
                                                        # before anything binds: --discord DISCORD_TOKEN +
                                                        # JUDGE_OPERATOR_DISCORD, the network roles
                                                        # JUDGE_OPERATOR_EMAIL, --web a built web/dist, --mcp
                                                        # MCP_TOKEN, --admin ADMIN_TOKEN + a built page; a
                                                        # token with no --mcp only warns. The network
                                                        # roles share API_ADDR (:8787) and GET
                                                        # /api/health. --jobs refreshes the database it is
                                                        # pointed at: JUDGE_REFRESH_HOURS=0 in a dev .env
npm --prefix web run build           # build the SolidJS page into web/dist (served by judgebot --web)
npm --prefix web run dev             # Vite dev server, proxies /api to a local judgebot --api
                                     # and /admin/api to a local judgebot --admin (:8789)

cargo build --release -p judge-agent                    # target/release/judge-cli + judge-mcp (build before
                                                        # .mcp.json can start judge-mcp)
judge-cli judge "<q>" [--thread T] [--pin span=Name]    # built-in pipeline: real spend, own cap per process
judge-cli begin "<q>" [--thread T] | prompt <s> | status <s> | extract <s> <file|-> | rules <s> <id>..
judge-cli verdict <s> <file|-> [--persist] | persist <s>   # the agent-driven session, step by step
judge-cli card <name> | card-info <uuid> | get-rules <id>.. | search "<q>" [--limit N] | glossary <term>
judge-cli config                                        # the resolved provider/model setup, secrets redacted
scripts/config.sh                                       # judge-config from the image (compose service `config`,
                                                        # no env_file, so it runs before .env exists): the way
                                                        # the docs tell operators to configure
cargo run --release -p judge-configure                  # judge-config: edit judge.toml + .env on 127.0.0.1:8790
                                                        # (prints a #token= URL; secrets write-only, never shown)
judge-mcp                                               # the MCP server on stdio (.mcp.json starts it)
                                                        # remote: judgebot --mcp serves /mcp (needs MCP_TOKEN)

cargo run -p judge-eval -- recall [--vectors]           # retrieval gate, no API keys (--vectors: the configured
                                                        # embedder, ~$0.001), exit≠0 below 90% retrieved or 75%
                                                        # shown under the synthesis budget
cargo run -p judge-eval -- answer --label L --limit 22 --max-usd 6.00   # full live gold run (~$1.70)
                                                        # --config judge.toml runs it on other providers
cargo run -p judge-eval -- grade eval/runs/<run>.json   # the synthesis model grades each answer: follows from
                                                        # its quotes? agrees with the reference? (~$0.40, D28;
                                                        # resumable; --force regrades; answer --grade does both)
cargo run -p judge-eval -- rescore eval/runs/<run>.json # re-score a stored run, zero API cost
cargo run -p judge-eval -- show eval/runs/<run>.json    # bot vs gold answers side by side
cargo run -r -p judge-eval -- sweep eval/sweeps/<plan>.toml [--dry-run]  # the way to compare models/efforts:
                                                        # extraction alone, synthesis on the gold extraction,
                                                        # named end-to-end configs; × repeats, one fixed grader,
                                                        # resumable, max_usd over the sweep's ledger.jsonl;
                                                        # `concurrency` questions × `parallel_runs` runs at
                                                        # once (4 × 3); eval/runs/sweep-<label>/summary.md
```

## Architecture

Pipeline (`docs/ARCHITECTURE.md` §3 is kept current):

1. **Extraction + classification.** One LLM call with structured output, at the model's
   measured effort (`judge_llm::EXTRACT_EFFORTS`: Opus 5.5 medium, Sonnet 5.5 low, Haiku
   5.5 high, unlisted low).
2. **Card resolution.** A typed resolution order: alias → possessive-stripped alias → exact →
   printed name → short-name-before-comma → epithet after the comma (two words or more) →
   alias-suffix → trigram fuzzy.
   - A `[[bracketed]]` span is `CardSpan::Exact` and takes only exact → printed name →
     whole-span alias (`[[bob]]` resolves: an alias names one card). A miss is offered
     only as `Ambiguous`: the possessive / short-name / alias-suffix hits under their own
     step (so a duplicate is dropped), else fuzzy neighbours.
   - The resolved cards are stamped onto `Verdict<Validated>` (`cards()`) and every
     interface shows them.
   - Resolution **never guesses**. Ambiguity becomes `Resolution::Ambiguous` and a Discord
     "did you mean?" button row.
3. **Retrieval.** Three sources unioned in priority order: the primary category's CR
   subsections ranked by text relevance, tsvector BM25, pgvector cosine, then the
   secondary categories. Primary subsections sharing no word with the question go after
   the next two sources. The synthesis budget renders a prefix, so this order is what the
   model reads. Retrieval also adds rulings for all faces, glossary, nightmare-card notes
   and rated prior calls.
4. **Synthesis.** At the model's measured effort (`judge_llm::SYNTH_EFFORTS`: Opus 5.5
   medium, Sonnet 5.5 low, Haiku 5.5 medium, unlisted high; a truncated answer reruns at medium, or low from medium),
   with citation validation and one retry. At most one
   `lookup_rules` tool round, enforced by typestate.
5. **Persist** + Discord rating buttons.

Crate graph (`core` ← `llm` ← `anthropic` and `openai` ← `embed` ← `bot` ← `api` ← `judgebot`,
and `bot` ← the other bins):

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
- `embed`: Voyage + OpenAI-compatible `/embeddings`, each an open `EmbedBackend` that
  reports usage. The sealed `WithSpace` everything downstream takes is implemented only by
  `MeteredEmbedder<B>` (reserve bytes + 16 tokens per text, settle to usage; prices in
  `VOYAGE_PRICES`, `EmbedPrice::{Free, Table, PerToken}`), so every embedding is behind
  the spend cap by type. Its requests and refusals have their own meter counters
  (`embedding_calls`, `embedding_refusals`): `calls()`, the ledger's `calls` and the
  questions-refused alert stay chat-only. A dropped `Reservation` keeps its worst case.
- `bot` (lib `judge_bot`): sqlx adapters, `config.rs` (the `judge.toml` loader),
  `extract.rs`/`synth.rs` over `judge-llm` only, prompts in `crates/bot/src/prompts/`,
  the serenity Discord layer with pure `render.rs` (`discord::serve`, the
  `--discord` role), `serving.rs` (`Serving`: what the serving roles of one process
  share), and `ingest.rs`, the data steps (Scryfall, CR, curated lists, embedding, emoji,
  `init`, `refresh`), which `jobs.rs` schedules, and `backup.rs`, the database backup
  (settings, `pg_dump`, a SigV4 S3 client over reqwest for any S3-compatible store (`BACKUP_S3_*`: endpoint,
  region, path-style or virtual-hosted; R2 one provider), the bucket-kept schedule).
- `api` (lib `judge_api`): the network roles. `Network` (made only by passing
  `ApiConfig::check`) and `run`.
- `judgebot`: the binary. `roles.rs` (`Role`, `plan`), `cli.rs` (the command line),
  `ingest.rs` (argument parsing over `judge_bot::ingest`), `backup.rs` (over
  `judge_bot::backup`).
- `eval`: a bin.
- `agent`: lib + `judge-cli` / `judge-mcp` bins. `judgebot --mcp` mounts its MCP handler.
- `configure`: `judge-config`, the localhost editor for `judge.toml` and `.env` (D23).
  - Its form is `config::file_schema()` (schemars over the loader's serde types), so the
    doc comments on the `File`-shape types in `config.rs` are operator-facing help text.
  - `EndpointKey::on` is the one endpoint × key table. The loader's misplaced check reads it.
  - Secrets are write-only: `DotEnv::replace` writes a value the page typed, and
    every reply goes through `server::redact`. No `BadValue` may quote a value.
  - `env::VARS` must list every `.env.example` variable as secret or setting
    (`registry_matches_the_example`), and a new variable needs a comment block directly
    above its line, which becomes its help.
  - A new `ConfigError` variant needs a `location()` arm. The match is exhaustive.
  - The page (`crates/configure/ui/`) is plain JS under a strict CSP, built with no
    `innerHTML`, linted by web's Biome in `check.sh web`. `app.js` is an ES module and
    its text is in `ui/messages.js` (one object per locale, held to English's keys by a
    Rust test); server text is `judge_configure::catalog`. A refused request (host or
    token) is answered without reading `.env`.

`judge_bot::build_deps(pool, Models, embedder)` is the composition root shared by the bot,
eval, the HTTP API and the agent's `judge` tool. `Models::{single, pair, priced}` take the
meter and bare backends and meter them themselves. Their fields are private, so there is
no uncapped model and no foreign meter. Every binary gets its `Models`/`Vectors` from
`config::Config::load()`. Its no-file branch (`from_vars`) is the zero-config setup:
Anthropic direct, one model for both stages, one `SpendMeter`.

`crates/bot/tests/anthropic_golden.rs` pins the four Anthropic request shapes
byte-for-byte against captured fixtures. `UPDATE_GOLDEN=1` re-captures them after an
intended prompt/schema change. Review the diff.

What a `judgebot` process does is a launch option, not a consequence of starting it
(`crates/judgebot/src/roles.rs`):

- `Role::{Discord, Api, Web, Mcp, Admin, Jobs}` is exhaustive, held in a `NonEmpty` set so
  "doing nothing" is unrepresentable. Flags on the command line win; with none,
  `JUDGE_ROLES` (the same flags).
- `roles::plan` matches every role and checks its requirements, all reported at once
  (`Unstartable`, a `NonEmpty<Problem>`), before the pool connects or anything binds.
  Its output is the types the roles run on (`discord::Config` holds the token,
  `DiscordOperator`, `judge_api::Network`, and for any serving role the `Models`, built
  there), so a role cannot start unchecked. `--jobs` alone with `JUDGE_REFRESH_HOURS=0`
  is refused: it would have nothing to do.
- `ApiConfig::check` matches every `Interface` and returns every `Unmet` (`Refused`):
  `--mcp` needs `MCP_TOKEN` and `--web` an `index.html`. The mirror case, a token with no `--mcp`, is a warning
  (`Network::warnings`). Refusing there would take a working page down over a variable
  that exposes nothing.
- One process, one composition: one pool, one `Config`, one `Models` (one `SpendMeter`,
  one `budget::start`), one `Vectors`, one migration. `jobs::start` runs only under
  `--jobs`. The HTTP listener binds (`Network::bind`) before Discord starts, then both
  run under `tokio::select!`: the first to stop ends the process non-zero. Each keeps its
  own `JUDGE_CONCURRENCY` slots.
- `jobs::start` returns a `Scheduler`, whose `ended()` completes when the thread ends.
  A jobs-only process awaits it and exits non-zero. Beside serving roles it is kept: if
  the thread ends on its own it logs ERROR and the process keeps answering; on a
  requested stop `main` waits for it (`Scheduler::stopped`, `Ended::Shutdown`).
- `roles` is the `judgebot` crate's library (`src/lib.rs`), so `judge-config`'s
  `check.rs` resolves the compose service's roles with `roles::compose_roles` and checks
  only the surfaces those roles run (no `DISCORD_TOKEN` needed without `--discord`).

`api` (+ the SolidJS page in `web/`) is the anonymous interface.

Also true of `api`:

- **The public feed** (D36, #26): `judge_bot::feed` is every public read of `calls`
  (`page`, `call`, `rateable`), each through `publicly_visible(private, at, window_days)`
  (one SQL function, migration `20261020000002`, inlined by the planner),
  username only (never `asker_id`); missing, private and too old are one `None`.
  `judge_api::feed` serves `GET /api/feed` (cursor `<µs>.<id>`, 20 a page) and
  `GET /api/calls/{id}` under `--web` unless `JUDGE_FEED=false` (`Config::feed`),
  with their own read limiter (`feed::READ_LIMIT`). `/api/about` carries `feed`, and
  under `--web` an unrouted `/api/` path is a JSON 404, not `index.html`. The web app
  routes `/feed`, `/calls/<id>`, `/users/<name>` in `web/src/router.ts`; `/help`
  mentions the feed unless it is off. `ratings` counts Discord and web apart
  (`Ratings { discord, web, judge }`); `reasons` only with `JUDGE_PUBLIC_FEEDBACK=true`
  (`feed::Reasons`), never the rater's identity. `/api/about` also carries `ratings`
  and `public_feedback`; a dismissed queue item's reason is left out of `reasons`, and
  `@everyone`/`@here` are broken (`feed::public_text`).
- **The dashboard** (D39, #28): `judge_bot::db::stats` is the one query module.
  `public` serves `GET /api/stats` (`judge_api::stats`, mounted with the feed,
  `READ_LIMIT` 30/min, 2 slots); `usage` serves `/admin/api/usage`, `admin_usage` and
  `judge-cli stats`. Public counts go through `publicly_visible`. Cost and model figures
  exist only via `CostAccess` / `ModelAccess` from `stats::Disclosure`
  (`JUDGE_PUBLIC_COST`, `JUDGE_PUBLIC_MODELS`); a `model` filter without models is a
  400. Run figures are null under a user filter. `/api/about` carries `stats`. Web:
  `/dashboard`, and the admin Dashboard at `/admin/usage`. Each report runs in one
  read-only transaction with a statement timeout (`PUBLIC_TIMEOUT` 5 s,
  `OPERATOR_TIMEOUT` 1 min); past it the public route answers 503 busy
  (`StatsError::TimedOut`). A public range that ends before `JUDGE_FEED_DAYS` is a 400
  (`BadRange::BeforeWindow`). The public and operator scopes are two `UNION ALL`
  branches, so a generic plan still uses the partial indexes. The report's budget is the
  `budget_setting` row that `budget::sync` (`record_setting`) writes beside
  `spend_days`; null until a process has written it.
- `mcp::router` gates with `route_layer`, not `layer`. `layer` wraps a router's fallback
  too, and merging that into a router with no web fallback made the 401 the catch-all for
  every unrouted path.
- Web ratings: `POST /api/calls/{id}/feedback` (`judge_api::rating`, beside
  `/api/judge`, D38) as the tab's session (`web:<uuid>`, the history id). The server
  decides replacing: an earlier rating without `replace: true` is a 409 `confirm`,
  nothing written. Without `replace` the write is `CallStore::rate_if_new`
  (insert-only, re-read on conflict → `RatedNew::Taken` → 409), so a race cannot
  replace unasked; Discord keeps `rate`'s upsert. Its own limiter (`rating::LIMIT`, 30 per 10 min), a 16 KiB body,
  `deny_unknown_fields`, a public in-window call only (`feed::rateable`), the IP via
  `client_ip` and the UA stored. `/api/judge`'s answer carries `call`.
- "Did you mean?" is stateless, via `pins` → `pin_card` rewriting. Session
  history is keyed by a client UUID (`web:<uuid>` thread ids).
- Per-IP fixed-window rate limiting (`API_RATE_LIMIT`/`API_RATE_WINDOW_SECS`) sits ahead
  of the concurrency semaphore and the spend cap.

Key cross-file facts that aren't obvious from any one file:

- **Citations are typed and validated.** `Citation::{Rule, ScryfallRuling, OracleText,
  PriorCall}` each carry a verbatim `quote` checked as a substring of the source in
  `Context`.
  - Key order in the schema is the order the model writes. With structured output enforced,
    the model emits keys in the schema's property order, so schemars runs with
    `preserve_order` (declaration order) and `Citation`'s `kind_first` transform moves the
    tag, which schemars appends, to the front, matching the prompt's
    `{"kind", "id", "quote"}`. Alphabetical order made Opus 5.5 write values one key over.
  - `Quote` cannot be blank. A blank one fails to parse, so a placeholder citation is a
    stub, not a bad citation (dropped, or a `MalformedCitation` with the stub notice when
    nothing else is cited: next bullet but one). Its schema is plain
    `String`. A quote that parsed but quotes nothing is a stub too (`verdict::stub_reason`:
    blank, a stock word such as "placeholder", or under `verdict::MIN_QUOTE_CHARS`), judged
    on the quote alone.
  - Stubs are **dropped, not rejected** (D21): `validate` sets them aside, parsed or not,
    and validates the rest as before. An answer with nothing but stubs is a
    `MalformedCitation`. An unreadable entry with a real quote still rejects.
  - A ruling citation with no such ruling whose quote is the *card's own Oracle text* is
    the commonest wrong citation (a card with no rulings in the material). It is still a
    `BadCitation`, never repaired, but `judge_core::misfiled_oracle_text` lets the retry
    notice say "cite it as `oracle_text`" instead of "drop it".
  - Its mirror: an `oracle_text` citation quoting the face's type line, mana cost or name,
    which the material prints on the `[oracle …]` label line and which is not citable.
    `judge_core::quotes_face_label` names the part, so the notice says so rather than
    "not a verbatim substring".
  - The check folds typographic punctuation (`judge_core::quote`): curly quotes, the dash
    block, non-breaking spaces. It maps one `char` to one `char`, never case or words.
    Models retype the CR's `’` as `'`, and that was the most common rejection.
  - What is stored is the *source's* span, not the model's string. A persisted quote
    stays byte-exact and the retirement pass's `citation_supported` stays a strict check.
  - The prose is held to the citations too (D20): a rule number written in the answer
    (`verdict::uncited_rules`, a regex plus `RuleId`) must be covered by a rule citation,
    the id itself, its rule or one of its sub-rules. Otherwise `JudgeError::UncitedRules`
    / `Rejection::Uncited`, checked last so a bad citation is reported first. Rulings and
    Oracle text have no id in prose, so this is rules only.
  - A failed check, or an empty/citation-less verdict on an answerable source, becomes a
    retry with the rejection rendered into the prompt. The rejection is logged at INFO,
    so a second failure can be read against the first. The retry is a fresh conversation,
    so the rejected answer is quoted back as a blockquote (`RejectedAttempt`), except for
    a placeholder or over-long answer.
  - The answer is an `Answer` newtype that decodes a stray `\n`, `\t` or `\"` written as
    two characters (Sonnet 5.5's habit) when the verdict is made (D22). Citations are not
    touched: a stray escape there fails the verbatim check.
  - Only `Verdict<Validated>` can reach `CallStore::persist` or Discord rendering.
- **CR chunking is two-granularity.** `rules` rows exist at rule level (`702.19`). That
  body includes all lettered sub-rules + examples, gets an embedding and feeds retrieval.
  They also exist as leaf rows (`702.19b`, `parent_id` set), which are the citation
  targets. Scoring and `lookup_rules` treat leaf↔parent as covering each other.
- **Ratings shape retrieval, nothing else.** The `calls_rated` view is a Bayesian mean
  (prior 2.0, weight 3) with a judge-role override (`effective_score`). Prior calls below
  1.5 are excluded (the crowd's score with ≥5 votes, a judge's at once), as are retired
  calls. Prior calls are always rendered
  *after* CR material as examples. `calls_rated` counts Discord ratings only
  (`rater_kind`). A rating's reason (`judge_core::Rating`, `feedback_queue`) reaches the
  operator's queue (`judge_bot::feedback`, ops `queue_*`, `/admin/api/queue*`), never
  retrieval.
- **Unreleased cards** (D42, `judge_core::preview`). `cards.first_printed` (the cards
  step: earliest `released_at` over default-cards printings) after `current_date` makes
  a card `Release::Upcoming`, read by `db::cards::load_cards` (the retirement pass uses
  `load_card_text`, unassessed). It is covered only when the loaded CR is dated at most
  `COVER_WINDOW_DAYS` (21) before it *and* every `rules_terms` entry (types, subtypes,
  keywords less ability/flavor words and mode names) appears in `rules`/`glossary` as a
  whole word. `CardRef::status` carries it onto the verdict: `Unreleased` gets the note
  above the answer on every interface (`render::unreleased_note`, web `unreleasedNote`,
  the agent's `note`), the material gets a line under the card's heading (released
  cards' material is unchanged), `calls.unreleased_cards` keeps it for the feed. The
  prior-call query skips a call with a noted card or about a card first printed after
  the call's day, for good.
- **A call is retired when its citations stop holding, not when the CR changes.**
  - `retire_unsupported` (`db/retire.rs`) is run by `ingest retire` and inside every
    refresh (both through `ingest::retire`, under the refresh lease). It re-runs `citation_supported` over every stored call against
    today's rules, rulings and Oracle text. It sets `calls.retired_at`/`retired_reason`
    both ways, so restored text brings a call back.
  - Each call also carries `context_ids.card_text` (an `oracle_fingerprint` per context
    card), so an erratum retires calls *about* the card even when they cited only the CR.
  - `cr_version` on a call is a record, not a gate.
  - Rulings are keyed by content (`ruling_key`, 16 hex chars), so a reindexed ruling is
    the same ruling. The key lives in core because the ingest writer and every reader
    must agree.
- **A renumbered rule keeps its calls.**
  - Inside the CR load transaction, `renumber_map` (`bot/src/ingest/renumber.rs`) matches old
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
    A session-persisted call is thread history only. The prior-call query skips
    `session_id IS NOT NULL` rows because nothing can rate them.
  - `Rejection` is adjacently tagged because it is stored.
  - The surface is `crates/agent`. `ops.rs` is the one list of operations, and `mcp.rs`
    and `bin/cli.rs` only transport. `.claude/skills/judge/SKILL.md` tells Claude Code how
    to drive it.
- **Vectors carry their space, and the database records the one it holds.**
  - Every embedder (`judge_embed::{VoyageEmbedder, OpenAiEmbedder}`) carries a `Space`
    (provider *kind* `voyage|openai`, model, dimensions), reached through the metered
    `WithSpace`. `Config::embedder()`/`vectors()` bill it to the config's meter;
    `embedder_billed_to(meter)` is for a reloaded config inside a process that has one
    (the scheduled refresh, `judgebot ingest`).
  - At the spend cap `Vectors::embed` returns `None` (WARN once per streak): retrieval
    loses its vector source and persist stores a NULL vector. The ingest embed loop fails
    naming the cap, keeping what it wrote.
  - The one-row table `embedding_space` names what the stored vectors are. Migration
    `20260904000001` seeds it `voyage/voyage-3.5/1024` for a DB that already held vectors.
    `Space::check` (pure, in `judge_embed::space`) is the only definition of "same space".
  - `ingest embed` writes the row on first use. It refuses on a mismatch or when the
    columns' actual `vector(N)` typmod differs (`db/space.rs` `column_width`). It never
    relabels vectors it did not write.
  - The adapters hold no bare `Embedder`. `PgRetriever`/`PgLibrary`/`PgCallStore` take
    `Arc<db::Vectors>` (`Config::vectors(pool)`, one per process), which embeds nothing
    until the stored space equals its own. A mismatch is an error-level log naming both
    spaces and vector search turned off, never a mixed column.
  - The row is re-read on every use, and once at startup so the verdict sits beside the
    config summary. A running bot picks up the first `ingest embed`, and a `reembed` under
    it turns vector search off instead of erroring or mixing.
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
- **Gold eval set** (`eval/gold.yaml`): 24 questions (21 adversarially verified, one a
  human correction of a reported wrong answer, two about invented unreleased cards: `fixture_cards`
  resolved by `eval::fixtures`, never in the database, and `unreleased: [card]`, checked by
  `UnreleasedCheck`) with `decisive_rule_ids` (what a correct
  answer must cite; `answer` scores recall on these), `supporting_rule_ids` (background,
  reported when cited, never a miss; `recall` gates on both) and per-question
  `equivalent_rule_ids` (alternate rule ids stating the same fact). The loader builds a
  validated `GoldQuestion` (`gold::Expected`): unknown keys are errors, the two lists
  are disjoint, and every equivalent key names an id in one of them. Extend the set when
  adding capability. `rescore` re-scores old runs after gold edits. Rule ids written
  unquoted in YAML are rejected, because floats drop trailing zeros.
- **Text is in typed catalogs** (D35): a struct with a field per message, one value
  per `judge_core::Locale` (only `En` ships), picked through `Catalogs::new(en, stub)`
  so a new variant breaks every surface: `judge_core::locale` (core),
  `discord::catalog`, `judge_api::catalog`, `judge_bot::alert::catalog`,
  `judge_agent::catalog`, judgebot's `catalog.rs`, `judge_configure::catalog`;
  `web/src/messages.ts` and `web/src/admin/messages.ts` (`satisfies Messages`),
  `site/src/i18n.ts`. The dev-only `stub-locale` feature adds `Locale::Stub` (`qps`,
  every message `⟦name values⟧`) so tests render every message in a second locale;
  `check_stub_locale` keeps it out of shipped builds. Command lines read the terminal
  locale (LC_ALL → LC_MESSAGES → LANG; codeset and modifier stripped, a `C`/`POSIX`
  value passes to the next), then `JUDGE_LOCALE`. No user-facing
  string inline elsewhere; logs stay English. `Locale` is negotiated per request
  (Discord user → `JUDGE_LOCALE`; web body `locale` → `Accept-Language` →
  `JUDGE_LOCALE`). `ids::Pick`/`Retry` end in `:<locale>` for any locale but `en` (absent = `en`, the
  shape older builds read), and
  `PICK_QUESTION_LIMIT`/`RETRY_QUESTION_LIMIT` are the minimum over all catalogs. The
  agent tools stay English (`ops::AGENT_LOCALE`): models read them.
- **Discord layer:** interaction logic is kept pure and unit-tested (`render.rs`,
  `ids.rs` typed button custom-ids, `feedback.rs` the rating form's JSON and its submitted
  values, `pick.rs` a pick prompt's age, digest and audience,
  `question.rs` span pinning, where `find_span` + `pin_at` equal `pin_card`). Replies
  open with a non-pinging `<@user> asked:` header. Rule citations link
  to the Yawgatog CR mirror (anchor = `R` + id with dots stripped). Rulings and Oracle
  text link to Scryfall search-by-oracleid, because the `/card/<uuid>` route 404s.
- **Card symbols are pictures on both interfaces.**
  - `discord/mana.rs` substitutes Discord application emoji (`{W}` → `<:mana_w:…>`).
  - `judge_core::symbol::emoji_name` is the one definition of the name. It lives in core
    because two programs (the bot and the `ingest emoji` uploader) must agree on it.
  - Text is carried as `mana::Rendered` segments rather than a `String`. A tag costs ~28
    of Discord's 2000/4096 characters and must never be cut in half, so plain text is the
    only cuttable segment.
  - An application with no emoji uploaded renders the literal `{W}`.
  - The one exception is a "did you mean?" prompt's question, which is restated
    verbatim (`{W}` stays literal) because the click reads it back from the message.
  - The table sits behind `mana::SharedSymbols`. `discord::symbols::watch`, a task
    spawned on `Ready`, lists the emoji and then checks every ten minutes. It lists them
    again after a refresh run whose `emoji` step may have uploaded (`runs::emoji_since`),
    after a failed listing, while the table is empty, and hourly regardless. No restart
    is needed.
  - The web app does the same job with Scryfall's SVGs (`web/src/Symbols.tsx`).
- **Providers are configuration, not code.** `judge_bot::config` loads `judge.toml` into
  typed structs.
  - The file is `JUDGE_CONFIG`, else `./judge.toml` if present, else the default setup from
    `.env`: Anthropic direct, `claude-opus-5-5` both stages, Voyage if keyed.
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
    - The cloud endpoints sit behind judge-anthropic's `aws`/`gcp` Cargo features. These are
      default on, forwarded from judge-bot's own features and named in the Dockerfile. A
      lean build cannot name these endpoints, and the loader says "not built".
    - Their credentials come from the platform chains, never `judge.toml`: SigV4 via
      aws-config (service `aws-external-anthropic` with the `anthropic-workspace-id`
      header, or `bedrock-mantle`) and ADC via gcp_auth. They are resolved lazily and
      probed once at startup by `Config::probe_auth`, so an empty chain fails there, not
      per question.
    - Only `direct` sends the `fallbacks` beta, which Anthropic documents for the Claude API
      only (re-checked 2026-09-28); a provider's `refusal_fallbacks` overrides that either
      way. Bedrock also masks `output_config.format`, tool `strict` and every
      `anthropic-beta`. Verified against the live docs 2026-09-02.
  - The chat backend `judge-openai` is chat completions with `Dialect` knobs:
    `structured_output`, `strict_tools`, `reasoning_effort`, `max_tokens_param`,
    `cache_hints`. Its embeddings side has `send_dimensions`.
  - A model on an `openai` provider must be priced (`[models.X.pricing]`, cache prices
    defaulting high from `input`) or the provider `pricing = "free"`. The built-in table
    (`judge_llm::PRICES`) lists current Anthropic models only and prices an unknown
    Anthropic one as the default (Opus 5.5), so a dearer one is under-counted.
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
- `VOYAGE_API_KEY`. Blank turns the vector search off, and the bot still works. A
  `judge.toml` `[models.embed]` overrides it, including OpenAI-compatible embeddings.
- `JUDGE_CONFIG`: optional path to a `judge.toml` (see above). It is a *host* path.
  - `cargo run` reads it as is.
  - `docker-compose.yml` bind-mounts it into `judgebot` and `refresh` at
    `/etc/judgebot/judge.toml` and points their `JUDGE_CONFIG` there
    (`${JUDGE_CONFIG:+…}`). Blank mounts the tracked example, which nothing reads.
  - So a `./judge.toml` in the repo root is read by `cargo run` but invisible to the
    containers until `JUDGE_CONFIG` names it.
  - Editing the mounted file's content is not a change `up -d` recreates for, so
    `docker compose restart judgebot`.
  - The `api_key_env` of every provider a stage names lives in `.env` too. A table no
    stage names is parsed, but its key is never read.
  - The cloud endpoints' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS` live in `.env` as well,
    never in `.env.deploy`, which `judgebot` does not read.
- `DISCORD_TOKEN`.
- `GUILD_ID` (instant command registration).
- `JUDGE_ROLE` (default "Judge").
- `JUDGE_MAX_USD`, `JUDGE_CONCURRENCY`.
- `JUDGE_ROLES`: `judgebot`'s roles when its command line names none, and the compose
  service's `command` (below).
- `JUDGE_AUTO_MIGRATE` (default true). When true, `judgebot` applies pending migrations
  at startup. `judgebot ingest migrate` is the explicit form.
- `JUDGE_REFRESH_HOURS` (default 24, `1..=720`, `0` = off): how often `--jobs` runs
  the data refresh (`jobs::Schedule`, validated at `Config` load). Set it to `0` in a
  development `.env`: otherwise `cargo run` with `--jobs` refreshes the dev database
  for real.
- `JUDGE_BUDGET_PERIOD`, `JUDGE_ALERT_WEBHOOK` (D19; the webhook also hears the
  scheduled refresh).
- `JUDGE_FEED` (default on), `JUDGE_FEED_DAYS` (unset/`0` = no window, `1..=3650`),
  validated at `Config` load (D36).
- `JUDGE_CALL_RETENTION_DAYS` (unset/`0` = keep, `1..=3650`, validated at `Config`
  load; D34): the refresh's `prune` step (`ingest::prune`) and `judgebot ingest prune`
  delete older calls with their ratings. `judge_runs` stay (`call_id` SET NULL), and
  the retirement pass treats a cited prior call that no longer exists as supported.
- `JUDGE_PUBLIC_COST`, `JUDGE_PUBLIC_MODELS` (default false, validated at `Config` load;
  D39): cost and model figures on the public dashboard.
- `JUDGE_PUBLIC_FEEDBACK` (default false, validated at `Config` load; D38): the feed
  shows raters' reasons without identity (raters are told: `rated_public`,
  `form_*_hint_public`, `rateFormNotePublic`); dismissing an item hides its reason.
- `FEEDBACK_IP_DAYS` (default 90, `1..=3650`, validated at `Config` load; D38): web
  ratings' ip/user_agent are cleared past it on every rating write (`PgCallStore`) and
  by the refresh's `sweep` step.

For the HTTP API it also holds:

- `API_ADDR` (default `0.0.0.0:8787`).
- `WEB_DIST` (read only under `--web`).
- `API_INTERFACES`: retired in 2.0.0. Nothing reads it, and a set value refuses the
  launch with the `JUDGE_ROLES` line that replaces it (`roles::refuse_api_interfaces`,
  also reached through `compose_roles`, so `judge-config` reports it).
- `API_RATE_LIMIT`, `API_RATE_WINDOW_SECS`.
- `API_CLIENT_IP` (`peer`, `proxy` or `cloudflare`, see below; IPv6 counted per /64).
- `MCP_TOKEN`: the credential for `/mcp`, which also needs the `--mcp` role. ≥24
  chars, bearer-checked before the protocol.
- `MCP_ALLOWED_HOSTS`: the `Host` values the MCP transport accepts, meaning the public
  hostname (`JUDGE_DOMAIN`, or the tunnel's).
- `JUDGE_DOMAIN`: the host name the `public` profile's `edge` serves (read by Caddy
  alone; judge-config checks it).
- `MCP_JUDGE_LIMIT`/`MCP_JUDGE_WINDOW_SECS`: `judge` runs per window through `/mcp`. This
  is the most a leaked token can spend.
- `MCP_ADMIN_TOKEN`: off unless set. The second `/mcp` credential, the only one that
  lists and runs the admin tools (D37); `MCP_TOKEN` stays judge-only.
- `ADMIN_ADDR` (default `127.0.0.1:8789`; compose sets `0.0.0.0:8789` in the container)
  and `ADMIN_TOKEN`: the `--admin` role's own listener and bearer token
  (`/admin/api/*`, the `/admin` pages; `judge-cli admin-url`). The three tokens are
  ≥24 chars and must differ. `ADMIN_PORT` (default 8789) is the host port the proxy
  publishes it on (`127.0.0.1:${ADMIN_PORT:-8789}:8789`).

The `judgebot` and `refresh` containers override `DATABASE_URL` to `db:5432` inside the
compose network. The image builds the web app and sets `WEB_DIST=/srv/web`. The
`judgebot` service's `command` is `${JUDGE_ROLES:---discord --api --web --jobs}`.
`roles::COMPOSE_COMMAND` holds that string,
`roles::compose_roles` is the same rule in Rust (judge-config's check uses it), and
`roles_match_the_compose_file` holds the file to it. Its healthcheck probes
`/api/health` only when `/proc/1/cmdline` (or `JUDGE_ROLES`) has an HTTP role, and
passes otherwise. Upgrading from the two-service file needs `up -d --remove-orphans`.
Without it, a running `judgebot-api` makes the new container fail on port 8787, and with
only `judgebot-bot` running `up -d` exits 0 and leaves two bots on one token. The fix
is `docker rm -f judgebot-bot judgebot-api`.

Deployment is self-hosted (D15, D41). `docs/DEPLOYMENT.md` is the runbook; §3 lists the
routes: local only (default), public HTTPS (`COMPOSE_PROFILES=public`), an own reverse
proxy, or a Cloudflare Tunnel. `db` and `proxy` publish on `127.0.0.1` only: `proxy` (Caddy) holds
`127.0.0.1:8787` and forwards to `judgebot:8787` (and `127.0.0.1:${ADMIN_PORT:-8789}` to
`judgebot:8789`, the admin listener, which neither the public site nor the tunnel points at), because `judgebot` has no
`container_name` and no port so a deploy can run two (D29). A release's migrations keep
the previous release working (expand, contract later; `CONTRIBUTING.md`). `proxy/Caddyfile`
dials `judgebot:8787` afresh for every request (`keepalive off`, retrying a refused dial
for 5 s): a kept-alive connection to a draining container lost POSTs in a drill. What
remains is a connection the kernel had queued for the old container in the instant it
closes its listener, which is reset (Caddy resends no POST body, so retrying cannot
save it; a config-changing `caddy reload` resets connections too, so the deploy does
not switch upstreams that way). It runs with `--watch` over the mounted `proxy/`
directory, so a Caddyfile edit needs no restart; `deploy.sh` validates the file first
and ends with a `caddy reload` of it, a no-op unless the proxy runs something else. Public
traffic reaches `judgebot:8787` through `edge` (profile `public`, `proxy/edge.Caddyfile`:
Caddy on 80/443, Let's Encrypt for `JUDGE_DOMAIN`, the same handover settings, validated
and reloaded by `deploy.sh` like the proxy), or `proxy:8787` (network alias `api`, the
old service name a tunnel may still point at) from the `cloudflared` service, which the
`tunnel` compose profile starts. The host's `127.0.0.1:8787` is the proxy's `:8788`
site. Both Caddyfiles set `grace_period 70s` (an answer takes up to a minute) and
compose gives `proxy` and `edge` `stop_grace_period: 80s`, so a stop or recreation
finishes requests in flight; `deploy.sh`'s `up -d --no-deps proxy` can take up to 70 s
once when the definition changed. `edge`'s entrypoint is `proxy/edge-entrypoint.sh`,
which refuses a `JUDGE_DOMAIN` by the same rule as judge-config's `check::is_host_name`
(a test runs the script against it; shellcheck covers `proxy/*.sh`). Deploy credentials live in
`.env.deploy` (`TUNNEL_TOKEN`, `BACKUP_S3_*`, `BACKUP_*`; a set `R2_*` is refused with
its rename). Only `cloudflared`, the `backup`
service and `scripts/backup-db.sh` read it, never the internet-facing `judgebot` (D15).
The weekly backup is the `backup` compose service (`judgebot backup serve`, profile
`backup`, D26): it reaches `db:5432` over the compose network with the image's `pg_dump`
16 (PGDG, `PG_MAJOR` in the `Dockerfile`), has no `env_file: .env`, and takes a backup
whenever the newest `judgebot-<stamp>.dump.gz` in the bucket is `BACKUP_EVERY_DAYS` old.
`scripts/backup-db.sh` is the cron alternative and writes the same objects, so `list`,
`fetch` and the restore drill work across both. With unusable settings (a retired `R2_*`
name among them) `serve` posts the problem names once to `JUDGE_ALERT_WEBHOOK` and stays
up refusing rather than crash-looping, since a restart reuses the same environment.
Restoring is far cheaper than
re-ingesting, which re-pays the embedder per row, so take a backup before
`ingest reembed --yes` (runbook in `docs/DEPLOYMENT.md` §7).

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` (socket address), `proxy` (`X-Judgebot-Client-IP`, `ClientIpSource::ProxyHeader`)
or `cloudflare` (`CF-Connecting-IP`). `client_ip` never reads
`X-Forwarded-For`, because Cloudflare *appends* to a caller-supplied header instead of
replacing it. Its first hop is attacker-chosen, which would hand every request a fresh
allowance against a paid endpoint. `cloudflare` is only sound when nothing can reach the
origin except Cloudflare. `proxy` is only sound while `edge` (which overwrites the header
and drops a caller's `CF-Connecting-IP`) or the operator's own proxy on the host is the
only public route: `proxy/Caddyfile`'s `:8787` (tunnel) and `:8789` (admin) sites strip
it; `:8788` (the host's `127.0.0.1:8787`) passes client headers through and is reachable
from every compose-network container, so nothing on the network may point at it and no
non-loopback host address may map to it. `client_ip` reads the header only as a single
copy from a loopback/private peer, which does not help where Docker rewrites the source
(userland proxy for IPv6, rootless Docker, Docker Desktop). Every limiter keys on
`limit::Bucket`: IPv4 per address, IPv6 per /64.

**Data refresh runs inside `judgebot --jobs`** (`judge_bot::jobs`, D24), every
`JUDGE_REFRESH_HOURS`. The compose `judgebot` service runs `--jobs` by default.

- `jobs::start` (after migrations and config load) spawns an OS thread with a
  current-thread runtime and its own `POOL_SIZE` pool, so steps never take a request's
  worker or pooled connection. Not isolated: the CR load and the retirement pass hold
  `CALLS_REWRITE_LOCK` exclusively, and a persist that writes a vector (before every
  reply) waits for it, as under cron. A panicking check is logged, alerted once, and
  the next one runs; a dead thread logs ERROR.
- Each check (1.5 to 3.5 min after start, then every 10 to 12 min) reads `migrate::skew` (ahead
  or behind: warn once, write nothing), then `runs::history` (no rules loaded: pause
  for `init`). The pure `jobs::due` decides on the DB clock: last success older than
  the interval and last attempt older than `jobs::backoff` (1 h × 2^(failed_streak−1),
  capped at the interval; a run that left no row is remembered in memory). Due:
  `try_lease` (held: skip), re-read under the lease, run
  `ingest::refresh(…, Caller::Schedule(&shutdown))` under a timeout.
- `ingest::refresh` (every caller) checks the lease and `migrate::skew` before each
  step, and abandons the run at `ingest::RUN_TIMEOUT` (3 h). A skew skips the remaining
  steps (`Skip::SchemaAhead|SchemaBehind`), and the run is `RunOutcome::Stopped`
  (stored `ok` null): no alert, no backoff. A scheduled run also stops on the process's
  shutdown (D40): the step in progress is cut off (`Skip::Interrupted`), the rest are
  `Skip::Shutdown`, and the run is `Stopped` whatever ran before. A stopped run is not an
  attempt in `runs::history`, so the next process's first check runs it. Single-step `judgebot ingest` commands call
  `ingest::ensure_writable` once.
- `runs::history` counts an unfinished row older than `runs::ABANDONED_AFTER`
  (3 h 10 min) as a failed run (`abandoned_started_at`, alerted once per streak). The
  scheduler drops a run at that same limit, closes its row as failed
  (`runs::close_dropped`) and releases the lease with a bound. Its pool carries
  `statement_timeout` 30 min and `lock_timeout` 15 min (`jobs::bounded`), because sqlx
  sends no cancel for an abandoned statement, and `client_connection_check_interval` 2 s
  (`scheduler_pool`), so a cut step's server-side statement ends, and its locks
  (`CALLS_REWRITE_LOCK`) free, once the thread's runtime closes the connection.
- `Trigger::Schedule` skips `embed` above `embed::UNATTENDED_CEILING` (800 rows) with
  `Skip::EmbedCeiling`. Manual runs have no ceiling.
- `jobs::alerts` posts the first failure of a streak (saying when it timed out), the
  recovery and a ceiling skip the previous run did not also hit, through
  `judge_bot::alert` (shared with `budget`).
- Development: the schedule is on by default, so `cargo run -p judgebot -- … --jobs`
  against a dev database refreshes it for real (downloads, a new CR, embeddings, emoji
  uploads). Set `JUDGE_REFRESH_HOURS=0` in a development `.env`.
- compose mounts `judgebot-ingest-cache` into `judgebot` too.

`scripts/refresh-data.sh` still runs the `refresh` compose service (profile `refresh`,
entrypoint `judgebot ingest`) for a manual run or an operator's own
cron (`JUDGE_REFRESH_HOURS=0`). It shares the lease and the record with the schedule.
`docker compose run` enables the profile itself, so `up -d` never starts it. CR release detection scrapes Wizards' rules page for
the `MagicCompRules <date>.txt` link and compares the date to the stored `cr_version`.
The CR loader nulls embeddings only for rules whose text changed, so a new CR costs the
embedder a few hundred rules. `aliases` and `notes` are repo data, compiled into
`judgebot` (`include_str!`) and loaded by `init`, or by `aliases` / `notes` (no argument:
the built-in copy; a file: the operator's). Each load records its source and the YAML's
SHA-256 in `curated_lists`, in the transaction that replaces the table. The refresh's
`lists` step (`ingest::lists`) reloads a built-in list whose digest is not the binary's,
or whose table no longer equals what the binary's copy resolves to today, and never
touches a file-sourced one (nor does `init`). A list with no record (loaded before the
table existed) is adopted as built-in only when its table equals what the current copy or
a `LEGACY` copy (`data/legacy/`) resolves to; anything else is kept with a WARN. A change
to a list's entries appends the previous file to `LEGACY`. A built-in load refuses with
no cards loaded.

**Ingest runs take the refresh lease** (`judge_bot::lease`).

- `judge_bot::lease` is a `Lease<K>` over a sealed `LeaseKey`: `Refresh`
  (`RefreshLease`, `REFRESH_LOCK`) and `Gateway` (`GatewayLease`, `GATEWAY_LOCK`). The
  key carries the lock, the `application_name` label and the error noun; the wait is
  per key (`RefreshLease::acquire`, bounded; `GatewayLease::stand_by`, unbounded), so
  neither lease can stand in for the other. The module docs list all three advisory
  keys (with `CALLS_REWRITE_LOCK`), and a `const` assertion keeps them distinct. A new
  key is a new marker type there.
- Every lease session (holding or waiting) gets `lease::SESSION_SETTINGS`: server-side
  TCP keepalives (10 s idle, 5 s × 3) and `tcp_user_timeout` 15 s, so a vanished
  client's lock is freed in ~25 s rather than the kernel's 2 h, and
  `statement_timeout`, `idle_session_timeout` (PG14) and `transaction_timeout` (PG17)
  0 (waits are bounded by `lock_timeout` and the client; an idle holder must not be
  ended). A `Setting` with `since` tolerates 42704 on an older server. A
  `try_acquire` miss leaves its pooled connection untouched. Bookkeeping queries are
  bounded by `lease::QUICK`. Session-level locks need a direct connection, never a
  transaction-mode pooler.
- `RefreshLease` holds the session-level advisory lock `REFRESH_LOCK` on a connection
  detached from the pool, labelled `judgebot refresh lease (<process>) since <UTC minute>` in
  `pg_stat_activity`. Only `ingest::try_lease` (no wait; a miss keeps the pooled
  connection) and `ingest::lease` (`Lease::try_acquire`, `RefreshLease::acquire`) make
  one. `lease` waits at most `LEASE_WAIT` (1 h), then fails naming the holder.
- Every data-writing step takes `&mut RefreshLease` and reads its pool from it, so an
  unlocked step does not compile and two steps cannot run under one lease at once.
  `init` migrates first, then takes it. `migrate` (its own lock) and `emoji` (no
  database) do not. `db::space::{record_space, switch_space}` are `pub(crate)` for the
  same reason.
- It is taken outside `CALLS_REWRITE_LOCK`, never while holding it, so the two cannot
  deadlock. A dropped lease closes its connection and Postgres frees the lock.
- `refresh` and `init` call `RefreshLease::check` before each step. A lost lease stops
  the run, and `refresh` records the remaining steps as failed.
- `ingest::refresh` records each run in `refresh_runs` (`ingest::runs`: a `StepReport`
  per step as tagged jsonb, ages read on the database's clock by `runs::history`). A
  failure to record never stops a step.
- The download clients (`ingest::http_client`) have a connect and a read (idle)
  timeout, so a stalled connection fails its step instead of holding the lease.
