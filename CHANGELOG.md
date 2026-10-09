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

### Added

- **The long-running process refreshes the data itself** (the `--jobs` role, which the
  compose `judgebot` service runs). Every `JUDGE_REFRESH_HOURS` (default
  24, 1 to 720, `0` = off) one of them runs the steps of `judgebot ingest refresh`: cards
  and rulings, a new CR release, retirement, embeddings, emoji. No host cron is needed.
  The schedule is kept in the database, so the processes, a restart and a cron run agree
  on it, and the refresh lease means a run happens once however many there are. A failed
  run is retried after an hour, then less often while it keeps failing. The run has its
  own thread, runtime and database connections, so its downloads and parsing take
  nothing from answering. While a CR load or the retirement pass holds the calls lock,
  saving an answer still waits for it, as under a cron run. A process whose image does
  not match the schema pauses its schedule with one warning instead of writing, and a
  database with no rules loaded waits for `init`. A bad `JUDGE_REFRESH_HOURS` stops
  every binary that loads the configuration, as `JUDGE_BUDGET_PERIOD` does.
- **`JUDGE_ALERT_WEBHOOK` hears the scheduled refresh**: the first failure of a streak
  (saying when it timed out), the recovery after one, a run whose process died, an
  embedding step the spend guard skipped, and a crashed check. Each is posted once, not
  on every retry.
- **A scheduled refresh never pays for a mass re-embed.** With more than 800 rows waiting
  for a vector it skips `embed`, logs the count and alerts. `judgebot ingest embed` (or
  `scripts/refresh-data.sh embed`) does it when that spend is expected. A manual or
  cron'd `judgebot ingest refresh` has no ceiling.
- **Every interface says how fresh the data is.** `GET /api/about`, the MCP `about` tool
  and `judge-cli about` carry `freshness`: the Comprehensive Rules release loaded, the
  seconds since the last successful refresh, and whether the latest one failed. It is
  `null` when the database does not answer, and the rest is served regardless. The web
  footer shows it as one line and `/help` as a **Data.** list. `/api/health` ignores it.
- **`judge-cli stats` lists the last five refresh runs**: when each started, what started
  it and where, its outcome (`ok`, `failed`, `stopped`, `running` or `abandoned`), the CR
  version before and after, and the steps that failed.
- **The bot picks up new card-symbol emoji without a restart.** When a refresh run, by
  any process, uploads emoji, the bot lists them again within ten minutes. A bot with none
  checks every ten minutes, so the first `judgebot ingest emoji` after starting it is seen
  too, and every bot lists them hourly regardless. A listing that failed at startup is
  retried the same way, and connecting no longer waits for it.
- **One process answers Discord, however many run.** A `judgebot --discord` takes a
  gateway lease in the database before it connects. A second one on the same database
  (a replica, a new container started before the old one stopped) logs
  `standing by: another instance holds the Discord gateway`, naming the holder, serves
  its HTTP roles meanwhile, and connects 15 seconds after the holder exits, or about
  25 seconds more when the holder vanished without closing its connection (power loss,
  a partition). The holder checks every 5 seconds; when its lease is lost (a database
  restart) it disconnects and stands by again instead of restarting the process, so the
  two never answer the same question. Losses in quick succession pause before
  reconnecting, up to ten minutes, to keep within Discord's daily login limit. `DATABASE_URL` must be a direct connection, not a
  pooler in transaction mode. The old `bot` image does not take the lease, so the
  upgrade below still needs `--remove-orphans`.
- **`judgebot ingest init` is recorded as a refresh run** of the steps it shares with one
  (it now runs the retirement pass too). The schedule counts a first load as fresh data,
  and the steps it did not reach after a failure are recorded as skipped.

### Changed

- **One binary, `judgebot`, whose roles are launch options.** `bot`, `api` and `ingest`
  are now one program. `judgebot --discord --api --web --mcp --jobs` runs any non-empty
  set of the bot, the JSON route, the web page, the MCP transport and the scheduled
  refresh in one process; with no flags it reads `JUDGE_ROLES` (the same flags). Each
  role's requirements are checked before anything connects or binds, and every unmet
  one is named at once: `--discord` needs `DISCORD_TOKEN` and `JUDGE_OPERATOR_DISCORD`,
  the network roles `JUDGE_OPERATOR_EMAIL`, `--web` a built page, `--mcp` an
  `MCP_TOKEN`, and any serving role a chat model that builds. The HTTP listener is bound
  before the Discord gateway is contacted, and the first serving role to stop ends the
  process with a non-zero status. The data command line is `judgebot ingest <command>`, with the same
  commands and arguments as before. `judgebot --help` and `judgebot ingest --help` list
  them.
- **One compose service, `judgebot`, in place of `bot` and `api`.** It runs
  `JUDGE_ROLES` when that is set in `.env`, else `--discord --api --web --jobs`: what the
  two services ran together. `JUDGE_ROLES='--api --web --jobs'` runs it without Discord.
  The container is `judgebot` (it was `judgebot-bot` and `judgebot-api`), so the logs are
  `docker compose logs judgebot`. It keeps the `api` service's port, healthcheck and
  network name, so a Cloudflare Tunnel pointing at `http://api:8787` needs no change.
  `refresh` runs `judgebot ingest`, with the same arguments as before.
- **`API_INTERFACES` is deprecated.** While `JUDGE_ROLES` is unset the compose service
  still runs `--discord --jobs` plus its interfaces, and logs a warning naming the
  `JUDGE_ROLES` line that replaces it. `judge-config` shows the same warning and checks
  only the roles the service would run, so a deployment without Discord needs no
  `DISCORD_TOKEN` there.
- **One spend meter per process.** The roles of one `judgebot` process bill to one meter
  and one ledger. With `JUDGE_BUDGET_PERIOD=process`, a process running both the bot and
  the HTTP interfaces has one `JUDGE_MAX_USD` between them, where the separate `bot` and
  `api` processes had one each. `day` and `month` budgets were already shared. Each role
  keeps its own `JUDGE_CONCURRENCY` slots and the API its rate limits.
- **The scheduled refresh runs only under `--jobs`.** The compatibility names below keep
  it on, as before. A process whose only role is `--jobs` exits non-zero if the
  scheduler's thread ends, and refuses to start with `JUDGE_REFRESH_HOURS=0`. Beside a
  serving role, a scheduler thread that ends is logged as an error and the process
  keeps answering.
- **`judge-bot`, `judge-api` and `judge-ingest` are compatibility names.** The image
  carries them as links to `judgebot`, which runs what each ran (`judge-bot` is
  `--discord --jobs`, `judge-api` its `--api`/`--web`/`--mcp` rules plus `--jobs`,
  `judge-ingest` is `judgebot ingest`) and logs a warning naming the replacement. They
  ignore `JUDGE_ROLES`. A later release removes them.
- **A refresh stops writing when it should.** Before each step `judgebot ingest refresh`
  checks that the migration ledger matches its binary, and every single-step command
  (`cards`, `rules`, `embed`, …) checks once before it starts. A run between
  `docker compose pull` and `up -d` (schema behind) or on an old image (schema ahead)
  writes nothing, names `judgebot ingest migrate` or the newer image, and exits non-zero.
  It is recorded as stopped, neither a success nor a failure. A run stops after three
  hours, abandoning the step in progress, so a hung download cannot hold the lease.
- **A truncated CR download is never loaded.** The CR file is cached through a `.part`
  file and a rename, and a text that ends before its Credits section is refused (and
  its cached copy removed) instead of being loaded, which would have deleted every rule
  it did not reach.

- **Data refreshes take a lock in the database.** Every `judgebot ingest` command that
  writes data (`refresh`, `init` after its migration, `cards`, `rules`, `aliases`,
  `notes`, `embed`, `reembed`, `retire`) first takes the refresh lease, a Postgres
  advisory lock. It waits up to an hour for a run that holds it, logging who holds it,
  then fails. The cron run, a manual step and a workstation's `judgebot ingest` therefore
  take turns instead of overlapping. Postgres drops the lock with the session, so a
  killed run leaves nothing behind. `scripts/refresh-data.sh` no longer takes its
  `.refresh.lock` directory.
- **Downloads time out.** A step's connection to Scryfall or Wizards gives up after 30
  seconds without connecting or two minutes without data, so a stalled connection fails
  that step instead of hanging the run.
- **Each `refresh` is recorded** in a new `refresh_runs` table: start and finish, the CR
  version before and after, each step's outcome and whether all succeeded. A run that
  loads a new CR logs `CR <old> → <new>`.
- **A refresh with no embedder configured logs `refresh step skipped` for `embed`.** It
  used to log `refresh step ok`. `emoji` with no `DISCORD_TOKEN` was already logged as
  skipped.

### Upgrading

- **Without Discord, set `JUDGE_ROLES` first.** The one service runs
  `--discord --api --web --jobs` by default. A deployment that ran only `api`, with no
  `DISCORD_TOKEN`, adds this line to `.env` before upgrading, or `judgebot` restarts
  forever with `DISCORD_TOKEN is not set` (the error names this line too):

  ```ini
  JUDGE_ROLES='--api --web --jobs'
  ```

- **Deploy with `--remove-orphans`.** The compose file's `bot` and `api` services are
  now one `judgebot` service, and `up -d` alone leaves the old `judgebot-bot` and
  `judgebot-api` containers running. With `judgebot-api` running, the new container
  fails to start because `judgebot-api` holds port 8787. With only `judgebot-bot`
  running (a Discord-only deployment that started `bot` alone, or a stopped or
  crash-looping `api`), `up -d` exits 0 and `judgebot-bot` keeps answering Discord beside
  the new container on the same token, so every question is answered twice: the old
  image does not take the gateway lease.
  `--remove-orphans` removes both before the new one starts. Pull the image straight
  after `git pull`: a cron'd `scripts/refresh-data.sh` in between would run the new
  compose file's `judgebot ingest` on the old local image, which has no `judgebot`.

  ```sh
  git pull && docker compose pull            # the new docker-compose.yml and its image
  docker compose up -d --remove-orphans
  docker ps -a --filter name=judgebot- --format '{{.Names}} {{.Status}}'
  ```

  The last command should list `judgebot-db` (and `judgebot-tunnel` with the tunnel)
  and no `judgebot-bot` or `judgebot-api`. If either is there,
  `docker rm -f judgebot-bot judgebot-api` removes it.
- **The roles carry over.** A `.env` with neither `JUDGE_ROLES` nor `API_INTERFACES` runs
  `--discord --api --web --jobs`, what the two services did. One with `API_INTERFACES`
  runs `--discord --jobs` plus those interfaces and logs a deprecation warning: replace it
  with the `JUDGE_ROLES` line the warning names. A deployment without Discord: see the
  first item (add `--mcp` if it served `/mcp`).
- **The tunnel needs no edit.** The service answers to the network name `api` as well
  as `judgebot`, so a public hostname whose service is `http://api:8787` keeps working.
- **`scripts/refresh-data.sh`, its cron entry and `docker compose run --rm refresh
  <command>` keep working.** Other commands that named a service change: `docker compose
  logs judgebot`, `docker compose restart judgebot` after a `judge.toml` edit, `docker
  compose run --rm --entrypoint judge-cli judgebot …` for `judge-cli`.
- **A pinned `JUDGE_IMAGE_TAG` moves with the compose file.** `git pull` brings the new
  `docker-compose.yml`, which runs `judgebot`, a binary older images do not have. With
  the tag pinned to an earlier release (`1.2`, `1.2.0`, a `sha-` tag from before this
  one), `up -d` fails with `judgebot` not found: move the pin to this release in the same
  step, or keep the previous compose file until you do.
- **Rolling back needs the old compose file too.** An image from before this release has
  no `judgebot` binary, so the new `docker-compose.yml` cannot start it. Restore both:
  `git checkout <the previous release's tag or commit> -- docker-compose.yml`, set
  `JUDGE_IMAGE_TAG` to that release, then `docker compose pull && docker compose up -d
  --remove-orphans` (without `--remove-orphans`, the `judgebot` container stays up beside
  the restored `bot`).
- **Using compatibility names on purpose.** A compose file of your own that runs
  `judge-bot` or `judge-api` keeps working on the new image: each name runs what it ran,
  plus `--jobs`, and logs one warning naming its `judgebot` replacement.
- **Running from source, the binary is `judgebot`.** `cargo run -p judge-bot` becomes
  `cargo run -p judgebot -- --discord --jobs`, `cargo run -p judge-api -- --api --web`
  becomes `cargo run -p judgebot -- --api --web` (add `--jobs` for the schedule), and
  `cargo run -p judge-ingest -- <command>` becomes `cargo run -p judgebot -- ingest
  <command>`. `target/release/` holds `judgebot` in place of `bot`, `api` and `ingest`.
- **The schedule is on by default.** An instance with no recorded refresh runs one
  within minutes of starting. An instance that never had cron catches up that way. A
  development `.env` pointing `cargo run` at a local database should set
  `JUDGE_REFRESH_HOURS=0`, or that database is refreshed for real.
- **A cron'd `scripts/refresh-data.sh` keeps working.** It takes the same lease and
  writes the same record, so the schedule counts its run and never overlaps it. Remove
  the cron entry whenever convenient. To keep cron in charge instead, set
  `JUDGE_REFRESH_HOURS=0`.
- **`judgebot` mounts the `judgebot-ingest-cache` volume** the `refresh` service
  already used.
- The `refresh_runs` migration applies at startup unless `JUDGE_AUTO_MIGRATE=false`. A
  refresh that runs before it is applied works and logs that the run went unrecorded.
  The schedule waits for it, with one warning naming `judgebot ingest migrate`.
- A `.refresh.lock` directory left in the repository root by a killed run is no longer
  read and can be removed.

## [1.2.0] - 2026-10-08

### Added

- **Claude Haiku 5.5 is measured and priced.** The built-in price table knows
  `claude-haiku-5-5` and both of its rate cards ($0.10 input, $0.50 output per million
  tokens, and five times that for every token of a request whose prompt passes 100K), so
  a `judge.toml` naming it needs no `pricing` table. With no `effort`, it synthesizes at
  `medium`. On the gold set it cost five cents for the run: none of its answers
  contradicted the reference, 14 of the 18 in-scope questions were answered in full, 2
  in part, and 2 got "did you mean?" (`eval/published/v1-haiku-5-5.json`). Sonnet 5.5
  stays the recommended budget option. The README's results and the Model choice page
  compare all three.

- **`judge-config`, a config editor on localhost.** It edits `judge.toml` and `.env`
  from forms generated from the loader's own types. The help
  text comes from the files' documentation. Each draft runs through the loaders the
  binaries run at startup, and the panel says whether the database settings, the models,
  the Discord bot and the HTTP API would start, naming the key to fix if not. Only changed
  lines are written, after a diff. Secrets are write-only: never shown, and replaced from a
  masked input. `scripts/config.sh` runs it from the image (a `config` compose service
  that needs no `.env`, so it is the first setup step), and `cargo run -p judge-configure`
  from source. The setup guides now use it, with hand-editing `.env` as the alternative.
  See the Config editor page.

### Changed

- **The eval binary is `judge-eval`**, the name the documentation already used. It was
  built as `eval`.
- **Log wording.** The resolver's `card resolved` / `card ambiguous` lines name the
  matching step as `step=` (was `rung=`), and the embedding-space lines say `vector search
  off` / `on` (was `vector legs`). A log query filtering on the old text needs updating.
- **`.env.example` gains commented `ANTHROPIC_BASE_URL`, `VOYAGE_MODEL` and
  `VOYAGE_DIMENSIONS` lines**, and each comment block now sits directly above the
  variables it describes. No variable's meaning or default changed. Its
  `API_INTERFACES` example is quoted: uncommented as it was (`--api --web` bare),
  every `cargo run` binary refused the `.env` at startup. Compose was unaffected.
- **The image builds the web page and the docs on Node 26** (was 24). CI and the
  contributor setup moved with it. The site holds TypeScript at 6 until
  `@astrojs/check` accepts 7.
- **Dependencies are current.** Cargo, the web page and the docs site take the pending
  Dependabot updates, and `yoke-derive` moves past a yanked release that `cargo deny`
  refused.
- **Contributor tooling.** The pre-push hook runs only the groups the pre-commit hook
  leaves out (`sqlx`, `test` and `lint`), and `scripts/clean-target.sh` trims a
  `target/` directory that has grown large. Neither affects a deployment.

### Fixed

- **A local image build no longer copies `.env` into a layer.** `COPY . .` took the
  operator's `.env`, and a `.env.deploy` if present, into the builder stage. The build
  context now leaves out both and keeps only `.env.example`, which `judge-config`
  compiles in. Images published from CI were not affected, since a clone has no `.env`.

### Upgrading

Nothing is required: `docker compose pull && docker compose up -d` is the whole upgrade,
and no migration is involved.

- **`judge-config` needs a 1.2 image.** `scripts/config.sh` runs the editor from the
  image, so a deployment with `JUDGE_IMAGE_TAG` pinned to `1.1` or `1.1.0` has to move
  to `1.2` (or `latest`) first.
- **A log query on `rung=` or `vector legs`** needs the new wording (`step=`, `vector
  search off` / `on`), as described above.
- **A copy of `.env.example` kept as a `.env`** with `API_INTERFACES` uncommented bare
  (`--api --web`) should quote it, as the file now does.

## [1.1.0] - 2026-09-29

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
  Opus 5. On the gold set, at high effort on this release's code, every answer it gave
  agreed with the reference, for about 60% of the default's cost per answer. It
  answered 17 of the 18 in-scope questions and asked "did you mean?" on the other
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
- **`judge-eval answer` records the retry.** Each row says why the first attempt was
  rejected when the retry ran, and how many stub citations were dropped, and the table
  totals both by kind. Citations are stored with their whole quote. `rescore` and `show`
  still read older run files, and say those did not record it.
- **Synthesis effort follows the model.** With no `effort` on `[models.synth]`, Claude
  Opus 5.5 now synthesizes at `medium` (it was `high`), Claude Sonnet 5.5 stays at
  `high`, and any other model runs at `high`. On the gold set, Opus at medium answered all
  18 in-scope questions in agreement with the reference with no wrong asides, as at high,
  for slightly less; Sonnet at medium misdescribed a card on one question and made three
  wrong asides (one run each). A `judge.toml` that sets `effort` is unaffected, and so is
  Bedrock's `anthropic.claude-opus-5-5`, which is not in the table. An answer cut off at
  `max_tokens` is still retried at medium, and now at low when it was already medium.
  `judge-eval answer` records the synthesis effort in the run file. The published Opus
  run, the README's results and the Sample answers page are the medium run.
- **The gold set separates decisive rule ids from supporting ones.** `eval/gold.yaml`
  lists what a correct answer must cite (`decisive_rule_ids`) apart from background it
  may leave out (`supporting_rule_ids`). `judge-eval answer` and `rescore` score recall on
  the decisive ids and report supporting ids that were cited; `recall` still gates on
  both. The old `expected_rule_ids` key, like any unknown key, is now an error.

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
- **Examples were printed under the wrong rule.** A rule-level excerpt printed all of a
  rule's examples, and its sub-rules', after its last sub-rule, without their
  `Example:` label. An example of 903.3 therefore read as part of 903.3e and was cited as
  903.3e; 174 of the CR's 277 examples were out of place. Each example now sits labelled
  directly under the line it belongs to, in the model's material, the `lookup_rules`
  result and `/rule`. A quote of rule text filed under a neighbouring rule still gets a
  retry notice naming the rule that holds it. An existing database is corrected by the
  next CR release, or at once by reloading the current one (below).
- **Backslashes in answers.** Sonnet 5.5 sometimes wrote line breaks as a literal `\n`
  (and quotes as `\"`), which Discord and the web page showed as written. The answer's
  stray escapes are now decoded when the verdict is made (D22). Calls stored before this keep them.

### Upgrading

- **A `judge.toml` naming `claude-opus-5`** needs a `[models.<stage>.pricing]` table
  ($5 input, $25 output), or the spend cap prices it as Opus 5.5 and under-counts it by
  about a fifth. The built-in table lists current models only.
- **Claude Platform on AWS no longer sends refusal fallbacks.** Set
  `refusal_fallbacks = true` on that provider to keep sending them.
- **Optional: reload the current CR** to move its examples under their rules now rather
  than at the next CR release. `rules latest` skips a version already loaded, so name the
  file: `docker compose run --rm refresh rules "<the .txt link on Wizards' rules page>"`,
  then `docker compose run --rm refresh embed`. About 150 rules are re-embedded (well
  under a cent). Stored calls keep their citations; the nightly retirement pass checks
  them against the new text as it does after any CR load.

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

[Unreleased]: https://github.com/sloshy/mtg-judgebot/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/sloshy/mtg-judgebot/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/sloshy/mtg-judgebot/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/sloshy/mtg-judgebot/releases/tag/v1.0.0
