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

2.0.0 is a major release. Its migrations cannot run beside 1.x, its compose file runs one
`judgebot` service in place of `bot` and `api`, and some JSON shapes change. Read
**Upgrading** before deploying it.

### Added

- **Unreleased cards** (D42). An answer about a previewed card that is not out yet opens
  with a note naming it when the loaded Comprehensive Rules may not cover it: on Discord,
  the web app (and the feed, as the note stood when the answer was given) and the agent
  toolbox's `note`. The note is left off during prerelease week once the set's rules
  update is loaded and mentions every type and keyword on the card. The model is told the
  same in its material. An answer about a card that was not out yet is never shown as an
  example, even after the release. The cards step records each card's first printing
  date, so the first refresh after the upgrade fills it in. The HTTP API's `cards` carry a
  `status` (`released`, `previewed`, `unreleased`). The gold set gains two questions
  about unreleased cards.
- **Public feed.** The web app lists answered questions from every interface at `/feed`,
  newest first, with where each was asked and the Discord asker's username (anonymous
  otherwise), filterable by interface and searchable (`q`: the question, the answer or
  the asker's username); `/calls/<id>` is one answer's page and `/users/<name>` one
  user's questions. Only questions asked private stay off it. New endpoints
  `GET /api/feed` and `GET /api/calls/{id}`, with their own read limit per address (120
  a minute); an entry's `ratings` are `{"discord": {...}, "web": {...}, "judge"}`.
  `JUDGE_FEED=false` turns it off; `JUDGE_FEED_DAYS` hides older calls without deleting
  them. `/help` says public questions may be listed. Under `--web`, an unknown `/api/`
  path is now a JSON 404 rather than the page.
- **Dashboard.** The web app's `/dashboard` page (a tab beside the feed) shows what the
  instance has been asked and how it went: questions per day by interface as a chart
  (with a table of the same figures), how they went (answered, a "which card?" question,
  card not found, out of scope, declined, failed), the rating mix (the web's on
  request), and the most-asked topics, cards and askers, over 7, 30 or 90 days or a
  year, by interface, and per Discord user (`?user=`, linked from their page). It counts
  only questions not asked private, within `JUDGE_FEED_DAYS`, and is served wherever the
  feed is. New endpoint `GET /api/stats?from&to&days&interface&user&model` (30 reads a
  minute per address), and `/api/about` gains `stats`. `JUDGE_PUBLIC_COST=true` adds
  what the answers cost, `JUDGE_PUBLIC_MODELS=true` which models answered them and a
  model filter; both are off by default.
- **Ratings with reasons.** On Discord, *Partially correct* and *Incorrect* now ask why:
  a form with the kind of problem (wrong conclusion, missed part of the question, a
  citation doesn't support it, wrong card, misread the question, shouldn't have
  refused, outdated, other) and, optionally, what's wrong and the right answer, up to
  1000 characters each. With `DISCORD_INTERACTIONS=http` it is one form; over the
  gateway the kind of problem is a menu first, then the form. *Correct* is recorded at
  once, as before. Rating an answer you already rated asks first (**Replace** or
  **Keep**), and pressing *Correct* again says it is already recorded. Rating buttons on
  answers posted before the upgrade get the new flow. The reasons go to the operator,
  never the model.
- **Rating on the web.** The chat, the feed and call pages have the same three buttons,
  and *Partially correct* and *Incorrect* open the same form under the answer (with an
  optional name). Rating again asks before replacing. A web rating is anonymous (the
  tab's session) and never counts toward the examples. The feed's counts are Discord's;
  "Include N from the web" adds the web's. New endpoint `POST /api/calls/{id}/feedback`
  (beside `/api/judge`, 30 ratings per address per ten minutes), and `/api/judge`'s
  answer carries the stored call's id (`call`). `JUDGE_PUBLIC_FEEDBACK=true` shows
  raters' reasons on the feed and call pages, without who gave them (raters are told
  so); a dismissed queue item's reason is taken off.
- **A new web app design.** The page reads as a conversation with the judge: questions
  and answers as messages, the question box docked at the bottom, citations as one
  section listing each one's label that opens to every quote, and replies that are not
  answers as titled notices. The header links to the documentation and has a theme
  switch (Auto, Dark, Light). The feed has a Compact view (one line per question, the
  default) and an Expanded one. Figtree is bundled; nothing is loaded from another
  site.
- **Citation kinds in the HTTP API.** Every citation in a reply or a feed entry carries
  `kind` (`rule`, `ruling`, `oracle_text`, `prior_call`), so a client can tell them apart
  without reading the label, which may be translated.
- **Translation groundwork.** Every message the Discord bot, the web app, the admin
  pages, the HTTP API, the alert webhook, `judge-config`, the command lines' usage and
  the docs site's header and footer show now comes from a per-language catalog, and
  each request picks its language: the Discord user's; the browser's on the web (sent as
  the new optional `locale` field of `POST /api/judge`, else `Accept-Language`) and in
  `judge-config`; the terminal's (`LC_ALL`, `LC_MESSAGES`, `LANG`) for `judge-cli`,
  `judgebot` and `judge-config`. `JUDGE_LOCALE` sets the fallback, and the language of
  the alert webhook's messages. Only English ships, so nothing reads differently yet.
  `GET /api/about` gains `locale` and `locales`. A "did you mean?" or Retry button in
  another language will carry it in its id; English ones keep their shape. The
  answers, the rules material, JSON output, the agent tools' text and diagnostics (logs,
  the command lines' errors, health checks) stay English.
- **Admin interface.** A new role, `--admin`, serves the operator's pages on a port of
  its own (`ADMIN_ADDR`, default `127.0.0.1:8789`) behind a new `ADMIN_TOKEN`: every
  stored call, private ones included, filterable by interface, asker, model, visibility
  and text; one call with its asker, its ratings and their raters, and its run's models,
  tokens and cost; the failed calls; the feedback queue; and the Dashboard below.
  `judge-cli admin-url` prints the page's address with the token. In compose the
  `proxy` service publishes it on the host's `127.0.0.1:8789` only (`ADMIN_PORT` moves
  the host port); from another machine, forward the port over SSH. The public listener
  answers `/admin` paths 404, and the admin listener serves `GET /admin/api/about`. Off
  unless `JUDGE_ROLES` names it.
- **The admin Dashboard.** The public dashboard's report over every call, private ones
  included and counted apart, by range, interface, asker and model, with the failed
  calls, spend per day and month, the budget period, the cap and what is left, the runs
  by model, interface and stage, cost per answer, latency, the dearest runs and the runs
  the cap refused. `judge-cli stats` and the MCP tool `admin_usage` return the same
  report and take the same range and filters (`--from`, `--to`, `--interface`,
  `--asker`, `--model`). The budget is the instance's: every process that keeps the
  spend ledger records its period and cap beside it, so `judge-cli stats` no longer
  reports its own. A report's queries are cut off past 5 seconds on the public page and
  a minute for the operator.
- **Admin reads for agents.** `judge-cli calls` and `call <id>`, and the MCP tools
  `admin_calls`, `admin_call`, `admin_failures` and `admin_usage`, on stdio `judge-mcp`
  (and so over SSH) and on `/mcp` for a new `MCP_ADMIN_TOKEN` only, never `MCP_TOKEN`.
  Every admin call over `/mcp` is logged with its tool's name.
- **The feedback queue.** Every rating given a reason is an item for the operator,
  beside the call's question, answer, citations and cards: the admin page's Feedback
  tab, `judge-cli queue`, `queue-show`, `queue-resolve` and `queue-dismiss`, and the
  admin MCP tools `queue_list`, `queue_show`, `queue_resolve` and `queue_dismiss`,
  filterable by status, kind of problem, interface and kind of rater. An item is
  resolved (with a note and what fixed it) or dismissed (with a note); a rater who
  replaces their reason reopens it. `judge-cli stats` counts the queue by kind of
  problem (`feedback`) and web ratings apart from Discord's. The `judge` skill walks an
  agent through it.
- **`FEEDBACK_IP_DAYS`.** How long a rating given on the web keeps the address and user
  agent it came from (default 90 days, 1 to 3650). Every rating written clears older
  ones, and so does a new refresh step, `sweep`, between `prune` and `retire`.
- **Every call records where it came from.** A stored call names its interface
  (Discord, web, MCP, `judge-cli` or an agent session) and, on Discord, the asker's user
  id and username at asking time. `judge-cli stats` counts questions by each interface
  and how many were private.
- **Private calls are stored.** A `/judge private:True` answer is now kept, marked
  private, and has rating buttons. Only the operator sees it: it is never read as a
  later question's history, never used as an example, never embedded and never on the
  feed. The MCP `judge` tool and `begin_session` take `private`, and `judge-cli judge`
  and `begin` take `--private`. `/forget` now also removes the user's id and username
  from their questions, and a private question's text.
- **Failed calls are kept for troubleshooting.** A call that fails for a reason the
  operator has to diagnose is stored in `failed_calls` with the question, the error and
  the answer text of each synthesis attempt (each cut to a fixed length; the newest 500
  rows, 30 days), with its interface and asker. A question asked with `private: True`
  is stored too, flagged. `judge-cli failures` prints `{"failures": [...]}`, each row
  with its `id`. `/forget` anonymizes a user's rows (id, question, error and text
  removed) as well as deleting their ratings.
- **Every judge run is recorded with its cost.** A `/judge`, a web question, an MCP
  `judge` and a `judge-cli judge` each leave a `judge_runs` row, whatever the outcome
  (answered, ambiguous, not found, out of scope, refused, at the spend cap, failed),
  private ones included, with one `judge_run_charges` row per model call and embedding:
  the stage, the model the response named, its tokens and its cost. Each run's cost is
  its own, not a difference of the process's running total, so concurrent questions no
  longer bleed into each other's `usd=` log figure. A response that reports no usage is
  recorded at the worst case the spend cap kept for it, flagged as an estimate. No text
  or user id is stored there.
- **Call retention.** `JUDGE_CALL_RETENTION_DAYS` (unset or `0`: keep every call, as
  before) deletes answered calls older than that many days, with their ratings, at each
  refresh (a new `prune` step) or with `judgebot ingest prune`. Each run's usage record
  (`judge_runs`) and the daily spend ledger stay. `judge-cli stats` counts questions and
  ratings from the stored calls, so its counts for days past the retention drop to
  what is left. A deleted call stops being an example for later answers; a newer call
  that cited it keeps its citation. A rating button on a deleted answer says it is no
  longer stored.
- **Spend warnings before the cap.** `JUDGE_SPEND_WARN_AT` lists percentages of
  `JUDGE_MAX_USD` (such as `50,80`). Each is logged and posted to `JUDGE_ALERT_WEBHOOK`
  once per period when spending reaches it. In a `day` or `month` period one process
  sends each warning, however many share the budget. Unset by default.
- **Discord interactions over HTTP.** With `DISCORD_INTERACTIONS=http`, Discord POSTs
  slash commands, button clicks and forms to `/discord/interactions` (the application's
  Interactions Endpoint URL, a public HTTPS address such as the tunnel's) instead of
  sending them over a gateway connection. The bot then opens no gateway connection, and
  Discord keeps being answered while a deploy switches containers. `gateway` stays the
  default. The bot reports a setting that disagrees with the Developer Portal in its
  log and to `JUDGE_ALERT_WEBHOOK`. Steps: `docs/DEPLOYMENT.md`, "Discord over HTTP".
- **Deploys without downtime.** `scripts/deploy.sh` starts the new `judgebot` container
  beside the running one, waits until Docker reports it healthy, then stops the old one.
  The web app and the API stay up throughout. Discord is quiet for the new container's
  login, a few seconds: the old one records that it closed its gateway, so the new one
  connects without the 15-second grace a takeover after a crash waits (table
  `gateway_handover`). A new container that is not healthy within five minutes is
  removed and the old one keeps serving. `--build` builds from the checkout instead of
  pulling.
- **`scripts/deploy.sh` deploys a major release too.** It asks the new image what its
  migrations are (`judgebot ingest upgrade`, a new command that prints
  `upgrade: in-place`, `major` or `major-from-1.x`). For a major it takes a backup
  when one is configured, starts the new container beside the old one, stops the old
  one as soon as the new one listens, and waits for the migration.
- **Major upgrades answer with a maintenance reply.** A release whose migrations cannot
  run beside the previous release (a new major) starts its roles at once and answers
  every question with a maintenance message: on Discord the question with a **Retry**
  button, on the web a **Try again** button, over the API a `maintenance` reply (503,
  `Retry-After`), over `/mcp` the message as the error. It migrates once every process
  of the previous release has stopped (a lock every `judgebot` with a role holds while
  it runs), then answers as usual. `/api/health` fails meanwhile. Releases within a
  major still migrate first and run beside the previous release.
- **`judgebot` refuses to start against a newer major's schema.** A rollback within a
  major still starts and runs, since those migrations only add. Across a major, or
  between two builds of a new major's pre-release, it exits naming both builds,
  because the schema can no longer hold what it reads and writes: run the newer build,
  or restore the backup taken before the upgrade. A 1.x image has no such check. A
  pre-release tag needs that exact version in the tree.
- **`judge-cli` and `judge-mcp` check the instance's release.** Each migration records
  the release and commit that ran it (table `instance_release`). Against a newer
  release of the same major the tools print a note and keep working. Against another
  major, another build of a new major's pre-release (such as `2.0.0-dev`), or a database
  missing one of their migrations, every operation fails and says what to update,
  naming the instance's commit. Between releases `main` carries a `-dev` version.
  Migrations within a major only add; dropping, renaming and rewriting wait for the
  next major (CONTRIBUTING).
- **`judgebot` shuts down gracefully.** On SIGTERM (`docker stop`) or Ctrl-C it stops
  taking new work, releases the Discord gateway at once, stops a scheduled refresh in
  progress, finishes the answers in flight within two minutes, writes the spend ledger
  and exits 0. A second signal exits at once. Before, `docker stop` waited ten seconds
  and killed it, and a question in progress got no answer. The compose service's
  `stop_grace_period` is 130 seconds.
- **The long-running process refreshes the data itself** (the `--jobs` role, which the
  compose `judgebot` service runs). Every `JUDGE_REFRESH_HOURS` (default 24, 1 to 720,
  `0` = off) one of them runs the steps of `judgebot ingest refresh`: cards and
  rulings, a new CR release, the built-in curated lists, retention, retirement,
  embeddings, emoji. No host cron is needed. The schedule is kept in the database, so
  the processes, a restart and a cron run agree on it, and the refresh lease means a run
  happens once however many there are. A failed run is retried after an hour, then less
  often while it keeps failing. The run has its own thread, runtime and database
  connections, so its downloads and parsing take nothing from answering. While a CR
  load or the retirement pass holds the calls lock, saving an answer still waits for
  it, as under a cron run. A process whose image does not match the schema pauses its
  schedule with one warning instead of writing, and a database with no rules loaded
  waits for `init`. A bad `JUDGE_REFRESH_HOURS` stops every binary that loads the
  configuration, as `JUDGE_BUDGET_PERIOD` does.
- **`JUDGE_ALERT_WEBHOOK` hears the scheduled refresh**: the first failure of a streak
  (saying when it timed out), the recovery after one, a run whose process died, an
  embedding step the spend guard skipped, and a crashed check. Each is posted once, not
  on every retry.
- **A scheduled refresh never pays for a mass re-embed.** With more than 800 rows
  waiting for a vector it skips `embed`, logs the count and alerts. `judgebot ingest
  embed` (or `scripts/refresh-data.sh embed`) does it when that spend is expected. A
  manual or cron'd `judgebot ingest refresh` has no ceiling.
- **Each refresh is recorded** in a new `refresh_runs` table: start and finish, what
  started it and where, the CR version before and after, each step's outcome and
  whether all succeeded. A run that loads a new CR logs `CR <old> → <new>`.
  `judge-cli stats` lists the last five: when each started, what started it, its
  outcome (`ok`, `failed`, `stopped`, `running` or `abandoned`), the CR version before
  and after, and the steps that failed. `judgebot ingest init` is recorded as one of
  the steps it shares with a refresh (it now runs the retirement pass too), so the
  schedule counts a first load as fresh data, and the steps it did not reach after a
  failure are recorded as skipped.
- **Every interface says how fresh the data is.** `GET /api/about`, the MCP `about` tool
  and `judge-cli about` carry `freshness`: the Comprehensive Rules release loaded, the
  seconds since the last successful refresh, and whether the latest one failed. It is
  `null` when the database does not answer, and the rest is served regardless. The web
  footer shows it as one line and `/help` as a **Data.** list. `/api/health` ignores it.
- **The refresh keeps the built-in alias and note lists current.** Each load of a curated
  list records its source in a new `curated_lists` table: `builtin` for `init` and for
  `judgebot ingest aliases` / `notes` with no file, `file` when you name one, with a
  digest of the YAML. A new refresh step, `lists` (after `rules`), reloads a built-in
  list when the binary's copy differs, so a release that improves `data/aliases.yaml` or
  `data/notes.yaml` no longer needs a manual reload. A list loaded from your own file is
  never touched, by the refresh or by `init`, which now keeps it with a warning: loading a
  file opts that list out of built-in updates until you load it again with no file. A
  built-in list loaded before the cards were, or naming a card added since, fills in by
  itself. The step's outcome is in `refresh_runs.steps` as each list's `current`,
  `reloaded`, `adopted`, `kept` (with a reason) or `failed`.
- **The bot picks up new card-symbol emoji without a restart.** When a refresh run, by
  any process, uploads emoji, the bot lists them again within ten minutes. A bot with
  none checks every ten minutes, so the first `judgebot ingest emoji` after starting it
  is seen too, and every bot lists them hourly regardless. A listing that failed at
  startup is retried the same way, and connecting no longer waits for it.
- **One process answers Discord, however many run.** A `judgebot --discord` takes a
  gateway lease in the database before it connects. A second one on the same database
  (a replica, a new container started before the old one stopped) logs
  `standing by: another instance holds the Discord gateway`, naming the holder, serves
  its HTTP roles meanwhile, and connects 15 seconds after the holder exits, or about
  25 seconds more when the holder vanished without closing its connection (power loss,
  a partition). The holder checks every 5 seconds; when its lease is lost (a database
  restart) it disconnects and stands by again instead of restarting the process, so the
  two never answer the same question. Losses in quick succession pause before
  reconnecting, up to ten minutes, to keep within Discord's daily login limit.
  `DATABASE_URL` must be a direct connection, not a pooler in transaction mode.
- **The weekly backup can run with no host cron.** The new `backup` compose service,
  off by default behind the `backup` profile (`COMPOSE_PROFILES=tunnel,backup`), dumps
  the database, uploads it to an S3-compatible bucket and prunes old copies, the same
  backup `scripts/backup-db.sh` takes, with the same `.env.deploy` settings, object
  names and format. It takes one whenever the newest backup in the bucket is `BACKUP_EVERY_DAYS`
  old (new, default 7), checking hourly, so a restart does not reset the schedule and a
  cron'd script run counts. A failed attempt is retried after an hour, then less often,
  and `JUDGE_ALERT_WEBHOOK` (in `.env.deploy`) hears the first failure of a streak, a
  failure at a different step, and the recovery. It reaches Postgres over the compose
  network with the image's own `pg_dump` 16 (new in the image, about 6 MB unpacked), so
  it needs no Docker socket, and it reads `.env.deploy` and nothing of `.env` but
  `JUDGE_LOCALE`. The upload carries the dump's SHA-256, which the store checks. Its pruning
  deletes only backup-named objects directly under the prefix, and never the two
  newest, however old, so a lapse longer than `BACKUP_KEEP_DAYS` cannot leave a single
  restore point (the script's pruning has no such floor).
- **`judgebot backup run | list | fetch <name> [file] | serve | check`.** The service's
  commands by hand: `docker compose run --rm backup run` takes one now, `list` prints
  the objects, and `docker compose run --rm --no-deps -T backup fetch <name> > <name>`
  downloads one for the restore drill. Each works on the script's backups too. `check`
  reads the settings and finds `pg_dump`, sending nothing. Settings that are not usable
  are posted to `JUDGE_ALERT_WEBHOOK` by name, and `serve` then stays up refusing
  rather than restarting with the same environment. `scripts/deploy.sh` runs `check`
  with the new image before it changes anything when the `backup` profile is on, and
  brings the `backup` service onto the new image once the deploy has succeeded.
- **`judge-eval grade`.** The eval's answers are graded by a model (the synthesis
  model): whether each answer's ruling follows from its quotes, and whether it agrees
  with the gold reference (D28). `judge-eval grade <run.json>` grades a stored run
  (resumable; `--force` regrades), `answer --grade` grades a new one. A graded run
  costs about $0.40.
- **An Academy Manufactor note and gold question.** The built-in notes cover
  Manufactor replacing the whole token creation (an effect making a modified Food makes
  a plain Food, Clue and Treasure), and the gold set has a 22nd question on it. The
  refresh's `lists` step loads the note by itself.
- **A nickname whose card name several cards share is left unresolved** (and logged), as
  notes and the card resolver already did, instead of pointing at whichever card the
  database returned first. A card's own name still beats the same name on another card's
  face. None of the built-in aliases is affected.

- **Public HTTPS without Cloudflare.** The compose file has a `public` profile:
  `COMPOSE_PROFILES=public` with `JUDGE_DOMAIN=judge.example.com` starts `edge`, a
  Caddy on ports 80 and 443 that serves `https://<JUDGE_DOMAIN>` with a Let's Encrypt
  certificate it gets and renews itself, in front of `judgebot:8787` with the same
  handover as the proxy, so deploys keep serving. Without the profile nothing changes:
  the stack answers on `127.0.0.1:8787` only. A Cloudflare Tunnel is now the
  alternative rather than the runbook's setup, and `docs/DEPLOYMENT.md` §3 lists the
  routes (local only, public HTTPS, your own reverse proxy, the tunnel). `API_CLIENT_IP`
  gains `proxy`: the rate limit counts `X-Judgebot-Client-IP`, which `edge` sets to
  the visitor's address and the proxy's other sites remove from callers, read only as
  one copy from a loopback or private peer. A reverse proxy of your own on the host can
  set it too. `judge-config` checks `JUDGE_DOMAIN` under `public` (the rule the `edge`
  container applies) and suggests `API_CLIENT_IP=proxy` with it and `cloudflare` with
  the tunnel, and `scripts/deploy.sh` checks and starts `edge` when the profile is on.
  Every rate limit now counts an IPv6 visitor by its /64 rather than the full address.
  Caddy finishes the requests in flight for up to 70 s when `proxy` or `edge` stops.

### Changed

- **Measured efforts per stage.** A `judge.toml` stage that names no `effort` now runs at
  the level its model was measured at on the gold set, for extraction as well as synthesis.
  The default setup is Claude Opus 5.5 at medium on both stages: extraction at low now and
  then wrote part of a card's name alone and asked "did you mean?", which medium never did,
  for about 3% more a question. Sonnet 5.5's synthesis default moves from high to low,
  which agreed with the reference as often for a quarter less, and Haiku 5.5 extracts at
  high. An extraction at high effort or above gets a 4000-token output ceiling by default.
  Set `effort` on a stage to keep the previous behaviour.

- **Backups go to any S3-compatible store.** The backup's settings were Cloudflare R2's
  and are now `BACKUP_S3_ENDPOINT`, `BACKUP_S3_BUCKET`, `BACKUP_S3_ACCESS_KEY_ID` and
  `BACKUP_S3_SECRET_ACCESS_KEY`, with two new optional ones: `BACKUP_S3_REGION` (the
  region requests are signed for, default `auto`, R2's) and `BACKUP_S3_ADDRESSING`
  (`path`, the default, or `virtual-hosted`, which AWS recommends). `docs/DEPLOYMENT.md`
  §6 gives the values for Cloudflare R2, AWS S3, Backblaze B2 and MinIO. The object
  names are unchanged, so a bucket's backups keep listing, fetching and setting the
  schedule. The `R2_*` names are refused, naming their replacements (Upgrading).
- **One binary, `judgebot`, whose roles are launch options.** `bot`, `api` and `ingest`
  are now one program. `judgebot --discord --api --web --mcp --admin --jobs` runs any
  non-empty set of the bot, the JSON route, the web app, the MCP transport, the admin
  interface and the scheduled refresh in one process; with no flags it reads
  `JUDGE_ROLES` (the same flags). Each role's requirements are checked before anything
  connects or binds, and every unmet one is named at once: `--discord` needs
  `DISCORD_TOKEN` and `JUDGE_OPERATOR_DISCORD`, the network roles
  `JUDGE_OPERATOR_EMAIL`, `--web` a built page, `--mcp` an `MCP_TOKEN`, `--admin` an
  `ADMIN_TOKEN`, and any serving role a chat model that builds. The HTTP listener is
  bound before the Discord gateway is contacted, and the first serving role to stop
  ends the process with a non-zero status. The data command line is
  `judgebot ingest <command>`, with the same commands and arguments as before.
  `judgebot --help` and `judgebot ingest --help` list them.
- **`judge-bot`, `judge-api` and `judge-ingest` are gone.** The image carries
  `judgebot` alone, and it is the image's entrypoint: with no arguments it runs the
  roles in `JUDGE_ROLES`.
- **One compose service, `judgebot`, in place of `bot` and `api`.** It runs
  `JUDGE_ROLES` when that is set in `.env`, else `--discord --api --web --jobs`: what the
  two services ran together. `JUDGE_ROLES='--api --web --jobs'` runs it without Discord.
  Its healthcheck probes `/api/health` when the process listens (an HTTP role, or
  Discord over HTTP) and passes otherwise. `refresh` runs `judgebot ingest`, with the
  same arguments as before, and `judgebot` mounts the `judgebot-ingest-cache` volume
  `refresh` already used.
- **A new `proxy` service publishes `127.0.0.1:8787`** (Caddy, `proxy/Caddyfile`),
  forwarding to `judgebot` on a new connection per request, and `127.0.0.1:8789` for
  the admin interface. The `judgebot` container publishes no port and has no fixed
  `container_name`, so that a deploy can run two of them. The address and a host
  reverse proxy in front of `127.0.0.1:8787` are unchanged. The network name `api` now
  belongs to the proxy, so a tunnel whose service is `http://api:8787` goes through it.
  Edits to `proxy/Caddyfile` load without a restart.
- **`API_INTERFACES` is retired.** `JUDGE_ROLES` replaces it. A `.env` that still sets it
  is refused at startup, and the error names the `JUDGE_ROLES` line that replaces it,
  rather than the process starting without the interfaces it chose. `judge-config`
  reports the same error, and checks only the roles the service would run, so a
  deployment without Discord needs no `DISCORD_TOKEN` there.
- **One spend meter per process.** The roles of one `judgebot` process bill to one meter
  and one ledger. With `JUDGE_BUDGET_PERIOD=process`, a process running both the bot and
  the HTTP interfaces has one `JUDGE_MAX_USD` between them, where the separate `bot` and
  `api` processes had one each. `day` and `month` budgets were already shared. Each role
  keeps its own `JUDGE_CONCURRENCY` slots and the API its rate limits.
- **The scheduled refresh runs only under `--jobs`.** A process whose only role is
  `--jobs` exits non-zero if the scheduler's thread ends, and refuses to start with
  `JUDGE_REFRESH_HOURS=0`. Beside a serving role, a scheduler thread that ends is logged
  as an error and the process keeps answering.
- **`judge-cli stats` has a new shape.** It is the admin Dashboard's report (and
  `admin_usage`'s, and `/admin/api/usage`'s). `by_day` lists every day of the range,
  oldest first, with the calls per interface (`discord`, `web`, `mcp`, `cli`,
  `agent_session`, and `agent` for calls stored before this release) and `private`; the
  ledger's spend and model calls per day moved to `spend_by_day` (with
  `spend_by_month` beside it). `ratings` is
  `{"discord": {"correct", "partial", "incorrect"}, "web": {...}}` and counts the
  ratings of the calls in the range, where it was an array of the ratings given in
  it. `days`, `questions`, `usd`, `retired_calls` and `lowest_rated` keep their
  meaning; everything else (`run_stats`, `budget`, `failed_calls`, `feedback`,
  `refresh_runs`, the dashboard's counts) is new.
- **Embeddings count toward the spend cap.** Every embedding request (a question's query,
  a stored call's vector, `judgebot ingest embed`/`reembed`/`init`/`refresh` and the
  scheduled refresh's embed step) now reserves its worst case against `JUDGE_MAX_USD`
  before it is sent and settles to the provider's reported usage, on the same meter as
  the chat calls, so one cap covers both. Voyage models are priced from Voyage's list
  (`voyage-3.5` $0.06, `voyage-3.5-lite` $0.02, `voyage-3-large` $0.18 per million
  tokens; an unknown Voyage model at the dearest, $0.18), from the first token: the free
  allowance is not subtracted. `[models.embed.pricing] input = …` overrides the list. At
  the cap a question is still answered, without vector search, and its call is stored
  without a vector for the next `embed` to fill; an `ingest` embedding run stops naming
  the cap and keeps what it embedded. `judgebot ingest` now writes the spend ledger, so a
  re-embed counts toward `JUDGE_BUDGET_PERIOD` and shows in `judge-cli stats`, combined
  with the chat spend (the ledger's call count stays chat calls only), and so does a
  `--jobs`-only process. `reembed`'s dry run prints the cost at the configured price and
  what is left under the cap, in place of the generic estimate, and `reembed --yes`
  refuses to clear the vectors when the re-embed's worst case does not fit. A refused
  embedding does not trigger the "refusing questions" webhook.
- **A chat response with no usage counts its worst case against the spend cap.** An
  OpenAI-compatible server that leaves `usage` out of a paid model's response, fills
  it with zeros, or sends a 2xx body that cannot be read was counted at $0. It now
  keeps the reservation taken before the request, as an embedding with no usage
  already did. A free model is unaffected.
- **A web or MCP question whose caller goes away stops at its next model call.**
  Closing the tab (or cancelling the MCP request) mid-answer used to drop the run
  outright: the request in flight was still billed, but nothing recorded it. Now that
  request finishes and is charged, no further model call is sent, and the run is
  recorded as `abandoned`. An answer already written is still stored.
- **Discord answers a command it cannot run.** A slash command this version does not
  have (Discord can still list a previous release's for a moment after a deploy) gets a
  private "I don't recognise that command" instead of no response, and `/judge` in a
  direct message a private "works in a server only". Replies ping nobody unless they
  mention someone on purpose.
- **"Did you mean…?" buttons survive a restart and a gateway hand-off.** A click is
  answered from the message the buttons are on, by whichever process holds the gateway,
  for ten minutes after the prompt was shown. Nothing about the question is kept in
  memory. The message restates the question exactly as asked, then lists the choices
  as before (a card name past 60 characters is shortened in the list, not on its
  button). A new `pick_claims` table makes each prompt answer one click (a second
  gets "A card has already been picked"). It stores the message id, when the prompt
  was shown and a short digest of the message, never the question, so a
  `private: True` question still leaves no text in the database. Claims older than a
  day are deleted as new ones are taken.
- **A `/judge` question is at most 1300 characters.** Discord enforces the limit in its
  command box, so the whole question fits in a "did you mean…?" message. A question that
  still does not fit (emoji and some other characters can count twice toward the
  message limit) gets the choices as a list, without buttons, and asks you to ask again
  with the card written as `[[Full Card Name]]`.
- **`POST /api/judge` answers an unreadable request in JSON.** A body that is not JSON,
  has the wrong shape, is too large or is sent without a JSON `Content-Type`
  keeps its 4xx status (400, 422, 413, 415) but now gets `{"kind":"error","message":…}`
  instead of a plain-text body, so a client can parse every reply the same way.
- **Data refreshes take a lock in the database.** Every `judgebot ingest` command that
  writes data (`refresh`, `init` after its migration, `cards`, `rules`, `aliases`,
  `notes`, `embed`, `reembed`, `retire`, `prune`) first takes the refresh lease, a
  Postgres advisory lock. It waits up to an hour for a run that holds it, logging who
  holds it, then fails. The schedule, a cron run, a manual step and a workstation's
  `judgebot ingest` therefore take turns instead of overlapping. Postgres drops the lock
  with the session, so a killed run leaves nothing behind. `scripts/refresh-data.sh` no
  longer takes its `.refresh.lock` directory.
- **A refresh stops writing when it should.** Before each step `judgebot ingest refresh`
  checks that the migration ledger matches its binary, and every single-step command
  (`cards`, `rules`, `embed`, …) checks once before it starts. A run between
  `docker compose pull` and `up -d` (schema behind) or on an old image (schema ahead)
  writes nothing, names `judgebot ingest migrate` or the newer image, and exits non-zero.
  It is recorded as stopped, neither a success nor a failure. A run stops after three
  hours, abandoning the step in progress, so a hung download cannot hold the lease.
- **A deploy stops the scheduled refresh in progress.** A container asked to stop (every
  `scripts/deploy.sh`, `docker stop`) cuts the step in progress off, records the run as
  stopped, releases the refresh lock and exits within the drain. The new container's
  first check runs the refresh again. A stopped run, by a shutdown or a schema change,
  does not delay the next one. In `refresh_runs.steps` the step cut off is
  `"reason": "interrupted"` and the ones after it `"reason": "shutdown"`. The
  schedule's database connections set `client_connection_check_interval` (2 s,
  Postgres 14 on).
- **Downloads time out.** A step's connection to Scryfall or Wizards gives up after 30
  seconds without connecting or two minutes without data, so a stalled connection fails
  that step instead of hanging the run.
- **A truncated CR download is never loaded.** The CR file is cached through a `.part`
  file and a rename, and a text that ends before its Credits section is refused (and
  its cached copy removed) instead of being loaded, which would have deleted every rule
  it did not reach.
- **A refresh with no embedder configured logs `refresh step skipped` for `embed`.** It
  used to log `refresh step ok`. `emoji` with no `DISCORD_TOKEN` was already logged as
  skipped.
- **Building from source needs Rust 1.99** (`rust-toolchain.toml`, the `Dockerfile`).

### Fixed

- **A card's epithet names it.** The words after the comma in a legendary card's name now
  resolve on their own when they are two words or more: "Nimble Pilferer" is Ragavan.
  When another card's name holds the same words, the bot asks: "Grand Arbiter" offers
  Dovin and Grand Arbiter Augustin IV, "mind sculptor" Jace and Way of the Mind Sculptor.
  Either way a question that names the card in full as well no longer stops on a "did you
  mean?". Before, such a span went to the fuzzy search, which never counts as the same
  card. A one-word epithet ("ravager") still asks, since players often mean another card
  by it.
- `scripts/refresh-data.sh` works on Compose v2.20 (Synology's Container Manager), which
  has no `docker compose run --pull`. It checks that the pulled image is present
  instead, and still never builds on the host.
- A retry after a rejected answer now tells the model to send the whole answer. A retry
  that came back with an empty `answer` ended the call as "empty verdict".
- The retry after an answer named a rule without citing it now gets that rule. A rule
  reached through a pointer ("see rule 111.10" in a glossary entry) was often not in the
  material, so the retry had nothing to quote and could not call `lookup_rules`.
- A judge's *Incorrect* now drops an answer from the examples the model is shown, at
  once. It used to need five ratings in all, like the crowd's, so a wrong answer only a
  judge had rated stayed in.

### Upgrading

This release cannot run beside 1.x, and 1.x cannot run on its schema. Plan a few minutes
of maintenance, take a backup, and update the checkout and the image together.

**Before the deploy**

- **Rename the backup's settings in `.env.deploy`.** They no longer assume Cloudflare
  R2. Keep each value: `R2_ENDPOINT` becomes `BACKUP_S3_ENDPOINT`, `R2_BUCKET`
  `BACKUP_S3_BUCKET`, `R2_ACCESS_KEY_ID` `BACKUP_S3_ACCESS_KEY_ID` and
  `R2_SECRET_ACCESS_KEY` `BACKUP_S3_SECRET_ACCESS_KEY`; `sed -i 's/^R2_/BACKUP_S3_/'
  .env.deploy` does all four. R2 needs nothing else: `BACKUP_S3_REGION` defaults to its
  `auto`. Rename the lines rather than filling in the new example's blank
  `BACKUP_S3_*=` lines as well: the last line of a name wins, a blank one too (the
  script and `scripts/deploy.sh` refuse a name set twice). A set `R2_*` stops the
  `backup` service, `scripts/backup-db.sh` and `scripts/deploy.sh`, each naming the
  lines, so rename them right after `git pull`, before a cron'd script next runs and
  before deploying. The deploy then recreates the `backup` service with them.
- **Take a backup.** It is the only way back to 1.x (see *Rolling back* below).
  `scripts/deploy.sh` takes one itself when `BACKUP_S3_BUCKET` is set in `.env.deploy`;
  otherwise `scripts/backup-db.sh`, or `docker compose run --rm backup run` once the
  new compose file and image are in place.
- **Update the compose file with the image.** This image cannot run the previous
  release's `docker-compose.yml`: its `bot` service starts `judgebot` with no roles, and
  its `api` and `refresh` services name binaries the image no longer has. Run `git pull`
  before or in the same step as `docker compose pull`, never the image alone, and pull
  the image straight after: a cron'd `scripts/refresh-data.sh` in between would run the
  new compose file's `judgebot ingest` on the old image, which has no `judgebot`. A
  `JUDGE_IMAGE_TAG` pinned to a 1.x release (`1.2`, `1.2.0`, an earlier `sha-` tag)
  moves to this release in the same step.
- **Set `JUDGE_ROLES` if the defaults are not what you ran.** A `.env` with neither
  `JUDGE_ROLES` nor `API_INTERFACES` runs `--discord --api --web --jobs`, what the two
  services did. `API_INTERFACES` is refused at startup: `--discord --jobs` plus its
  value is the line to write, so `API_INTERFACES='--api --mcp'` becomes
  `JUDGE_ROLES='--discord --api --mcp --jobs'`. A deployment that ran only `api`, with
  no `DISCORD_TOKEN`, sets `JUDGE_ROLES='--api --web --jobs'` (add `--mcp` if it served
  `/mcp`), or `judgebot` restarts forever with `DISCORD_TOKEN is not set`. The startup
  errors name these lines too. `judgebot ingest` and `judgebot backup` read no roles,
  so `scripts/refresh-data.sh` keeps running meanwhile.
- **Decide what the feed shows.** The feed is on by default and lists every call not
  asked private, including those asked before this release, with the asker's username
  where one was stored (earlier calls have none); the public dashboard lists the
  Discord users who asked most. To keep them unlisted, set `JUDGE_FEED=false` (which
  takes the dashboard off too) or a short `JUDGE_FEED_DAYS` in `.env` before
  upgrading.
- **Check port 8789 on the host.** The new `proxy` service publishes `127.0.0.1:8787`
  and `127.0.0.1:8789` (the admin interface, whether or not `--admin` runs). If
  something on the host already listens on 8789, set `ADMIN_PORT` in `.env` to a free
  port first, or the proxy cannot start and takes 8787 down with it.
- **The first deploy recreates the `proxy` service once.** Its host port now maps to a
  new site, and it drains for up to 70 s when stopped. While it is recreated,
  `127.0.0.1:8787` and the tunnel are down from the moment it stops listening until
  the answers it was serving finish (up to 70 s) and the new one starts; Discord over
  the gateway is not affected.
- **A Cloudflare Tunnel setup needs no change.** `proxy:8787` (and the `api` alias)
  behaves as before, `API_CLIENT_IP=cloudflare` still reads `CF-Connecting-IP`, and
  `127.0.0.1:8787` still answers. The `public` profile is opt-in and claims ports 80 and
  443 only when it is on. A reverse proxy of your own on the host that sets
  `CF-Connecting-IP` with `API_CLIENT_IP=cloudflare` keeps working; setting
  `X-Judgebot-Client-IP` with `API_CLIENT_IP=proxy` is now the documented way.
- **Leave `/discord/interactions` unchallenged if you turn on Discord over HTTP.**
  Discord's requests come from its servers, not a browser, so they cannot pass a
  challenge: a Managed Challenge rule or a Cloudflare Access policy that covers that
  path, or Bot Fight Mode at all (it covers the whole zone and cannot skip a path),
  fails Discord's URL check and every interaction. `DISCORD_INTERACTIONS` stays
  `gateway` unless you set it.

**New variables**, all optional, in `.env` unless noted (`.env.example` documents each,
and `scripts/config.sh` edits them):

- Roles and listeners: `JUDGE_ROLES`; `ADMIN_TOKEN` (needed by `--admin`, at least 24
  characters), `ADMIN_ADDR` (outside compose; compose sets `0.0.0.0:8789` in the
  container), `ADMIN_PORT` (the host port compose publishes it on, default 8789);
  `MCP_ADMIN_TOKEN` (the admin tools over `/mcp`). `MCP_TOKEN`, `MCP_ADMIN_TOKEN` and
  `ADMIN_TOKEN` must all differ: a process that reads two equal ones refuses to start.
- Discord: `DISCORD_INTERACTIONS` (`gateway`, the default, or `http`).
- Exposure: `JUDGE_DOMAIN` (the host name the `public` profile serves; `edge` refuses
  to start without it), and `proxy`, a new value of `API_CLIENT_IP`.
- The data: `JUDGE_REFRESH_HOURS` (default 24; `0` in a development `.env`),
  `JUDGE_CALL_RETENTION_DAYS` (default: keep), `FEEDBACK_IP_DAYS` (default 90).
- What the public sees: `JUDGE_FEED` (default on), `JUDGE_FEED_DAYS` (default: no
  window), `JUDGE_PUBLIC_FEEDBACK`, `JUDGE_PUBLIC_COST`, `JUDGE_PUBLIC_MODELS` (each
  default false).
- Spend and language: `JUDGE_SPEND_WARN_AT` (default: no warnings), `JUDGE_LOCALE`
  (default `en`). A bad `JUDGE_LOCALE` stops `judgebot`, the `judgebot ingest`
  commands that write data (they keep the spend ledger, whose alerts it sets the
  language of) and the `backup` service, which reads it from `.env` through Compose.
- `.env.deploy`: `BACKUP_EVERY_DAYS` (the `backup` service's interval, default 7),
  `JUDGE_ALERT_WEBHOOK` (the `backup` service's alerts; it reads only that file),
  `BACKUP_S3_REGION` (default `auto`) and `BACKUP_S3_ADDRESSING` (default `path`).
- Retired: `API_INTERFACES` (above); `R2_ENDPOINT`, `R2_BUCKET`, `R2_ACCESS_KEY_ID` and
  `R2_SECRET_ACCESS_KEY`, renamed `BACKUP_S3_*` (above).

A bad value of any of these stops every binary that reads it, at startup.

**The deploy**

- **Deploy with `scripts/deploy.sh`** (after `git pull`). It sees a major upgrade from
  1.x (`the new image's migrations: major-from-1.x`), takes the backup when one is
  configured, removes the 1.x `bot` and `api` containers, and starts `judgebot` alone.
  Discord gets a maintenance reply while it migrates; the web app, the API on
  `127.0.0.1:8787` and the tunnel are down until it has, when the deploy starts the
  rest. The 1.x containers have no SIGTERM handler, so Docker stops them after 10
  seconds, and an answer still in progress then is cut off. Later deploys within 2.x
  keep serving throughout (blue-green), and a later major takes the same command.
- **Or by hand, with `--remove-orphans`.** The compose file's `bot` and `api` services
  are now one `judgebot` service, and `up -d` alone leaves the old `judgebot-bot` and
  `judgebot-api` containers running. With `judgebot-api` running, the new container
  cannot start because `judgebot-api` holds port 8787. With only `judgebot-bot` running
  (a Discord-only deployment, or a stopped or crash-looping `api`), `up -d` exits 0 and
  `judgebot-bot` keeps answering Discord beside the new container on the same token, so
  every question is answered twice: the old image does not take the gateway lease.

  ```sh
  git pull && docker compose pull            # the new docker-compose.yml and its image
  docker compose up -d --remove-orphans
  docker ps -a --filter name=judgebot- --format '{{.Names}} {{.Status}}'
  ```

  The last command should list `judgebot-db` and `judgebot-proxy` (and
  `judgebot-tunnel` with the tunnel) and no `judgebot-bot` or `judgebot-api`. If either
  is there, `docker rm -f judgebot-bot judgebot-api` removes it. With the tunnel,
  `cloudflared` waits for `judgebot` to be healthy, which it is not while it migrates:
  if `judgebot-tunnel` is not running afterwards, run `docker compose up -d` again.
- **With `JUDGE_AUTO_MIGRATE=false`, migrate by hand before deploying.**
  `scripts/deploy.sh` refuses a major upgrade with the setting off, before it changes
  anything, because the new container would never migrate it; its message names these
  steps:

  ```sh
  docker stop judgebot-bot judgebot-api      # the 1.x containers
  git pull && docker compose pull
  docker compose run --rm refresh migrate    # from the new image
  scripts/deploy.sh                          # now an in-place deploy
  ```

  Until the migrations are applied, the feed, web rating, the dashboard and the admin
  dashboard read a SQL function they add (`publicly_visible`) and answer `500`.
- **Rebuild `judge-cli` and `judge-mcp` with the image.** A 1.x tool has no release
  check and fails on this release's schema with SQL errors.

**After the deploy**

- **Check the tunnel's service in Cloudflare:** the public hostname of your tunnel, under
  Zero Trust → Networks → Tunnels. `http://api:8787`, what the runbook said, needs no
  change: the name now belongs to the `proxy`. A service naming the 1.x container
  (`http://judgebot-api:8787`) stops resolving: change it to `http://proxy:8787`.
- **Nothing changes in the Discord Developer Portal.** The commands are the same six and
  re-register at startup; the rating forms need no permission or intent. Over the
  gateway a fault's rating shows a menu before the form, over HTTP one form (D38): to
  get the one-step form, switch to `DISCORD_INTERACTIONS=http` (`docs/DEPLOYMENT.md`,
  "Discord over HTTP").
- **Use the service name with Compose.** The `judgebot` container has no fixed name
  (Compose names it after the checkout's directory, such as `mtg-judgebot-judgebot-1`),
  so `docker compose logs judgebot`, `docker compose restart judgebot` after a
  `judge.toml` edit, and `docker compose run --rm --entrypoint judge-cli judgebot …` for
  `judge-cli`, instead of `docker logs judgebot-bot`. `scripts/refresh-data.sh`, its
  cron entry and `docker compose run --rm refresh <command>` keep working.
- **A compose file or script of your own** that runs `judge-bot`, `judge-api` or
  `judge-ingest` moves to `judgebot`: `judge-bot` is `judgebot --discord --jobs`,
  `judge-api --api --web` is `judgebot --api --web --jobs`, and `judge-ingest <command>`
  is `judgebot ingest <command>`.
- **Running from source, the binary is `judgebot`.** `cargo run -p judge-bot` becomes
  `cargo run -p judgebot -- --discord --jobs`, `cargo run -p judge-api -- --api --web`
  becomes `cargo run -p judgebot -- --api --web` (add `--jobs` for the schedule), and
  `cargo run -p judge-ingest -- <command>` becomes `cargo run -p judgebot -- ingest
  <command>`. `target/release/` holds `judgebot` in place of `bot`, `api` and `ingest`.
- **The schedule is on by default.** An instance with no recorded refresh runs one
  within minutes of starting, so one that never had cron catches up that way. A cron'd
  `scripts/refresh-data.sh` keeps working: it takes the same lease and writes the same
  record, so the schedule counts its run and never overlaps it. Remove the cron entry
  whenever convenient, or set `JUDGE_REFRESH_HOURS=0` to keep cron in charge. A
  development `.env` pointing `cargo run` at a local database should set
  `JUDGE_REFRESH_HOURS=0`, or that database is refreshed for real.
- **The first refresh settles the curated lists.** The alias and note lists loaded so
  far have no record of their source. For each, the `lists` step compares the table
  with what a built-in copy loads. When they match, the list is recorded as built-in
  (`adopted`) and kept current from then on. When they differ (you loaded your own file,
  or edited the table), it is left as it is, and every run logs a warning until you
  choose: `judgebot ingest aliases <file>` records your list as yours, and
  `judgebot ingest aliases` with no file takes the built-in copy. Likewise for `notes`.
  On a deploy host, mount the file:
  `docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml`.
- **To move the backup off cron**, add `backup` to `COMPOSE_PROFILES` in `.env`, add
  `JUDGE_ALERT_WEBHOOK` to `.env.deploy` if you want its alerts, and
  `docker compose up -d`. Then remove the cron entry for `scripts/backup-db.sh`, or keep
  it: the service counts the script's backups, so the two can run side by side, and the
  script keeps working unchanged. A `BACKUP_KEEP_LOCAL` the script used is a host path,
  which the container does not see: mount a directory for the service
  (`docs/DEPLOYMENT.md` §6) or leave it to the script.
- **An `openai`-kind `[models.embed]` needs a price.** An embedding model on a
  `kind = "openai"` provider now needs `[models.embed.pricing]` or the provider marked
  `pricing = "free"`; otherwise `judge.toml` fails to load with
  `models.embed: no price for <provider>/<model>`. For OpenAI's own
  `text-embedding-3-small`, add under `[models.embed]`:

  ```toml
  [models.embed.pricing]
  input = 0.02          # USD per million tokens
  ```

  A local server (Ollama, vLLM) takes `pricing = "free"` on its `[providers.<name>]`
  table instead. Voyage needs nothing. A `reembed` that costs more than what is left
  under `JUDGE_MAX_USD` now stops at the cap: run it as
  `docker compose run --rm -e JUDGE_MAX_USD=20 refresh reembed --yes`.
- A `.refresh.lock` directory left in the repository root by a killed run is no longer
  read and can be removed.

**What changes for clients and stored data**

- **More is stored about each question.** A Discord call stores the asker's user id and
  username, and a `private: True` answer is stored (for the operator only). `/help`
  says so. Earlier calls are labelled by interface from their thread id, have no asker
  and stay public. An MCP or `judge-cli` call from before the upgrade is labelled
  `agent`, because the two could not be told apart.
- **Earlier ratings keep their scores and have no reasons.** A *Partially correct* or
  *Incorrect* rating given before this release opens no feedback-queue item; it still
  counts toward the examples and in `judge-cli stats`' lowest-rated list.
- **"Did you mean…?" buttons sent before the upgrade answer "That choice has
  expired".** They held their question in the old process's memory, as a restart always
  lost it. Ask the question again.
- **HTTP clients:** `POST /api/judge` has a new reply, `{"kind": "maintenance",
  "message", "retry_after_secs"}` (status 503 with `Retry-After`), during a major
  upgrade; an answer gains `call` and each citation `kind`; an unreadable request gets a
  JSON `error` body. `GET /api/about` gains `freshness`, `locale`, `locales`, `feed`,
  `ratings`, `public_feedback` and `stats`.
- **`judge-cli` and MCP consumers:** `judge-cli stats` has the new shape above (a script
  reading `by_day[].usd` reads `spend_by_day` now, and `ratings` is an object).
  `judge-cli failures` (new) prints `{"failures": [...]}`. The MCP tools are a superset
  of 1.x's: `judge` and `begin_session` take `private`, and the admin and queue tools
  are listed only to `MCP_ADMIN_TOKEN` and stdio `judge-mcp`. During a major upgrade
  every `/mcp` operation fails with the maintenance message.

**Rolling back**

- **Back to 1.x means restoring the backup.** A 1.x image has no release check: on this
  release's schema it starts and fails on the queries the new schema broke. Stop
  `judgebot` first (left running, it would migrate the restored database again), then
  restore into an empty database, since `pg_restore --clean` cannot drop the 2.x
  tables that reference 1.x ones, and then start the previous compose file and image
  together. An image from before this release has no `judgebot` binary, so the new
  compose file cannot start it.

  ```sh
  docker compose stop judgebot
  docker compose run --rm --no-deps -T backup fetch judgebot-<stamp>.dump.gz \
    > judgebot-<stamp>.dump.gz
  docker compose exec -T db dropdb -U judgebot --force judgebot
  docker compose exec -T db createdb -U judgebot judgebot
  gunzip -c judgebot-<stamp>.dump.gz \
    | docker compose exec -T db pg_restore -U judgebot -d judgebot
  git checkout v1.2.0 -- docker-compose.yml  # and JUDGE_IMAGE_TAG=1.2.0 in .env
  docker compose pull && docker compose up -d --remove-orphans
  ```

  `scripts/backup-db.sh fetch <name>` downloads a backup without the `backup` service;
  a local `pg_dump -Fc` file restores the same way without `gunzip`.
  `--remove-orphans` removes the `judgebot` and `proxy` containers, which would
  otherwise hold port 8787 or answer Discord beside the restored `bot`.
- **Within 2.x, a rollback starts and runs:** migrations within a major only add. A 2.x
  image refuses to start on a later major's schema, naming both builds.

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
