# Deployment

The bot and API run on a machine you own, behind a Cloudflare Tunnel. There is no
public IP, no forwarded port and no cloud compute bill: `cloudflared` dials *out* to
Cloudflare's edge and traffic returns down that connection.

```
Browser ──https──> Cloudflare edge        [TLS, WAF, rate limiting, challenge]
                        ↕ outbound tunnel — no inbound port on the host
                   cloudflared ──http──> api:8787 ──> db:5432
                                         bot ──outbound WS──> Discord
```

Two properties this preserves that a serverless split would not: the Discord gateway
stays a long-lived connection (no HTTP-interactions rewrite), and the spend cap,
concurrency semaphore and rate limiter stay single-process in-memory values rather
than becoming distributed state. See `docs/ARCHITECTURE.md` for the pipeline itself.

Hostnames, paths and the image name below are placeholders (`judge.example.com`,
`/path/to/mtg-judgebot`, `ghcr.io/<owner>/<repo>`); substitute your own. The
maintainer's own instance, whose web page the README links, runs exactly this way.

## 1. Prerequisites

- A host that stays on, with Docker and the compose plugin. The stack *runs* in about
  200 MB RSS (Postgres ~157 MB, api and bot a few MB each), so 2 GB of RAM is ample.
  It never has to *build*: CI publishes the image and the host pulls it (§8). That
  matters because `cargo build --release` across ten crates plus a Vite build wants
  ~4 GB and real CPU, which a NAS does not have.
- Compose syntax here is held to what older bundled versions accept — Synology's
  Container Manager ships v2.20, which predates the `env_file` long form. `.env.deploy`
  must exist on any machine running the `tunnel` profile, and only there.
- No Rust toolchain on the host. The image carries its own migrations: `bot` and
  `api` apply pending ones at startup (`JUDGE_AUTO_MIGRATE`, on by default), and
  `docker compose run --rm refresh migrate` is the explicit form for an empty
  database or an operator who opted out. Restoring a dump (§2) brings the schema
  *and* the `_sqlx_migrations` ledger with it.
- A domain whose DNS is hosted **on Cloudflare**. Tunnel hostnames resolve only for
  records in the same Cloudflare account, so third-party DNS cannot CNAME to
  `<uuid>.cfargotunnel.com`; the free plan requires moving the whole zone.

## 2. Move the data (do this before anything else)

Restore a dump rather than re-ingesting. A cold rebuild re-parses the CR and the
Scryfall bulk file and re-embeds every rule through the embedding provider, which costs money.

```sh
# old host
docker exec judgebot-db pg_dump -U judgebot -Fc judgebot > judgebot.dump

# new host
docker compose up -d db
docker exec -i judgebot-db pg_restore -U judgebot -d judgebot --clean --if-exists \
  < judgebot.dump
```

Sanity-check the restore before moving on:

```sh
docker compose exec -T db psql -U judgebot -d judgebot \
  -c "select (select count(*) from cards) cards,
             (select count(*) from rulings) rulings,
             (select count(*) from rules where embedding is not null) embedded,
             (select provider||'/'||model||'/'||dimensions from embedding_space) space;"
```

`embedded` being 0 means the vector leg is off — re-run `scripts/refresh-data.sh embed`
rather than shipping a degraded retriever. So is `space` being empty, or naming a
model other than the one the bot is configured with: the bot's vector legs stay
dark (an error-level log line at startup and on the change, never a mixed column)
until the row and the configuration agree — `UPDATE embedding_space SET model = ...`
if the row is mislabelled, `scripts/refresh-data.sh reembed --yes` (paid: every row
again) to actually change models — and, with the row already right, the same command
only fills whatever is still empty.

## 3. Create the tunnel

In Cloudflare **Zero Trust → Networks → Tunnels**, create a tunnel (remotely managed)
and add a public hostname:

| Field | Value |
| --- | --- |
| Subdomain | `judge` |
| Domain | `example.com` |
| Service | `http://api:8787` |

`api` is the compose service name — cloudflared resolves it on the compose network, so
the API never needs a published port. Cloudflare creates the proxied
`judge CNAME <uuid>.cfargotunnel.com` record for you; it must stay **proxied**
(orange cloud), unlike every other record in the zone.

Copy the connector token into `.env.deploy` as `TUNNEL_TOKEN`.

## 4. Configure and start

Two files, deliberately separate:

```sh
cp .env.example .env               # app config: API keys, DISCORD_TOKEN, GUILD_ID
cp .env.deploy.example .env.deploy # deploy credentials: TUNNEL_TOKEN, R2_*
```

`.env` is the `env_file` for `bot`, `api` and `refresh`. `.env.deploy` is read only by
`cloudflared` and `scripts/backup-db.sh`, so a token that can rewrite the tunnel or
delete every backup never enters the environment of the internet-facing API. Both are
gitignored. Model credentials — `ANTHROPIC_API_KEY`, every `api_key_env` a `judge.toml`
names, the cloud doors' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS` — belong in `.env`:
they are exactly what `bot`, `api` and `refresh` read, and nothing else does.

In `.env`, set:

```ini
COMPOSE_PROFILES=tunnel     # `docker compose up -d` now includes cloudflared
API_CLIENT_IP=cloudflare    # rate-limit on CF-Connecting-IP
JUDGE_MAX_USD=...           # the backstop for anonymous traffic
```

If `COMPOSE_PROFILES` in `.env` doesn't take effect on an older Compose, pass
`--profile tunnel` explicitly instead.

Then bring it up. A restored database (§2) already carries the schema and the
migration ledger, so there is nothing to migrate:

```sh
docker compose up -d
curl -s localhost:8787/api/health
```

Starting from an *empty* database instead, `docker compose up -d` is enough: `bot`
and `api` create the schema at startup. The explicit form, for a look at what is
about to happen or with `JUDGE_AUTO_MIGRATE=false`, runs from the same image with
nothing but Docker (`run` starts `db` if it is not up):

```sh
docker compose pull
docker compose run --rm --pull missing refresh migrate   # never falls back to building on the host
```

(`sqlx migrate run --source crates/bot/migrations` from a workstation over an SSH
tunnel to the loopback-bound Postgres writes the same ledger with the same checksums,
but takes neither the refresh-job lock nor the ahead check; prefer the container form
on a live host.)

Use a full `docker compose up -d` whenever the compose file's `db` service changes:
`up -d --build bot api` deliberately leaves `db` alone, so a changed port binding or
healthcheck would otherwise persist indefinitely.

### Optional: another model (`judge.toml`)

Without a `judge.toml` the containers run Anthropic direct with `ANTHROPIC_API_KEY` and
Voyage with `VOYAGE_API_KEY`, as `.env` has them. To run on other providers — a cheap
model for extraction, Claude through your own cloud account, a local Ollama, an
OpenAI-compatible gateway — write one (`judge.example.toml` documents every knob; the
README's "Choosing a model" is the short version) and name it in `.env`:

```ini
JUDGE_CONFIG=./judge.toml      # a host path: what `cargo run` reads, and what compose mounts
```

`docker-compose.yml` bind-mounts that file read-only into `bot`, `api` and `refresh` at
`/etc/judgebot/judge.toml` and points the containers' `JUDGE_CONFIG` there, so the one
variable serves the host and the containers. With it blank the tracked
`judge.example.toml` is mounted instead, only so the mount has a source: the loader reads
nothing it was not pointed at, and the setup stays the `.env` one. Three consequences:

- **The `api_key_env` of every provider a stage names must be set in `.env`**, including
  for `refresh`: each binary resolves all three stages (`[models.extract]`,
  `[models.synth]`, `[models.embed]`) at load, so `refresh` fails its nightly `embed`
  step on a chat key it never uses rather than run half a configuration. A provider
  table no stage names is parsed but its key is never read. `.env` is the `env_file` for
  all three containers, so one line there covers them.
- **The cloud doors take credentials from the platform chain, not the file.** For
  `claude-platform-on-aws` and `bedrock`, either put `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`) in `.env`, or mount a credentials
  file and name it — the containers run as `nobody` with no home directory, so the
  default `~/.aws` location does not exist:

  ```yaml
  # docker-compose.override.yml (gitignored like judge.toml; compose merges it in by itself)
  services:
    bot: &aws
      volumes: ["/home/you/.aws:/etc/aws:ro"]
      environment:
        AWS_SHARED_CREDENTIALS_FILE: /etc/aws/credentials
        AWS_CONFIG_FILE: /etc/aws/config
        AWS_PROFILE: judgebot
    api: *aws
  ```

  For `vertex`, the same with a service-account JSON and
  `GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp/sa.json`. An IAM role scoped to invoking the
  model is enough; nothing here manages infrastructure. `refresh` needs none of this
  (embeddings are Voyage or OpenAI-compatible, never a cloud door), and none of it goes
  in `.env.deploy`, which `bot`/`api` do not read.
- **The startup log tells you what resolved.** Every binary logs one
  `config=... extract=... synth=... embed=... cap=$...` line, then — in `bot`, `api`,
  `eval` and `judge-cli`, which make chat calls — `cloud credentials resolved` per cloud
  provider (the chain is probed once at startup, so a host with no credentials exits
  there naming the provider and the door; `refresh` never probes, it makes no chat call)
  and `embedding space matches the database` or `embedding space mismatch` (below). `docker compose run --rm --entrypoint
  judge-cli api config` prints the whole resolution as JSON, secrets redacted.

A changed `judge.toml` is read at the next start, which means `docker compose restart bot
api` — not `up -d`: compose recreates a container only when its configuration or image
changed, and the content of a bind-mounted file is neither, so `up -d` prints `Running`
and leaves the old configuration in place. (Changing `JUDGE_CONFIG` itself in `.env`
does change the configuration, and `up -d` recreates.) The next `refresh` run picks the
file up on its own. Changing `[models.embed]` is the one edit that needs the database
moved too — see §7.

If an older Compose rejects the `${JUDGE_CONFIG:+…}` interpolation in
`docker-compose.yml` at parse time, set the containers' side by hand in the same
override file: `environment: {JUDGE_CONFIG: /etc/judgebot/judge.toml}` on `bot`, `api`
and `refresh`, with the mount left as it is.

### Optional: MCP for your own agents

`judge-api` can serve the judge's tool surface (`crates/agent`) to an MCP client over
the same tunnel, at `/mcp`. It is off unless `MCP_TOKEN` is set, and there is no
anonymous mode: every request must carry `Authorization: Bearer <MCP_TOKEN>` or gets a
401 before the protocol sees it. Behind the token are `judge` (the full pipeline, real
model spend, under the same `JUDGE_MAX_USD` and `JUDGE_CONCURRENCY` as the web
page), the agent-driven sessions (no model calls, database work only) and the
read-only lookups.

```ini
MCP_TOKEN=<openssl rand -base64 32>       # at least 24 characters, or the API refuses to start
MCP_ALLOWED_HOSTS=judge.example.com,localhost   # Host values accepted: the tunnel's hostname, plus
                                                   # localhost for curl on the host; the list replaces the default
```

`MCP_ALLOWED_HOSTS` matters: the MCP transport validates `Host` against a loopback-only
default (a DNS-rebinding guard), and cloudflared forwards the public hostname, so an
empty list means every `/mcp` request is refused with 403 while `/api/judge` keeps
working. Then `docker compose up -d api` and, from a workstation:

```sh
claude mcp add --transport http judge https://judge.example.com/mcp   --header "Authorization: Bearer <MCP_TOKEN>"
```

The per-IP rate limit of `/api/judge` does not apply to `/mcp` (the token is the
identity). Instead `judge` runs through `/mcp` are capped per window
(`MCP_JUDGE_LIMIT` per `MCP_JUDGE_WINDOW_SECS`, default 20 an hour), on top of the
shared `JUDGE_CONCURRENCY` slots and `JUDGE_MAX_USD` cap. That cap is the blast radius
of a leaked token: about `MCP_JUDGE_LIMIT × $0.12` an hour, and never the whole spend
cap or every judge slot at once, so the public page keeps working. Sessions and
lookups make no chat-model call; they do embed the question or the search text with
the configured embedder when there is one (fractions of a cent, and uncapped — the only
paid upstream without a cap). Rotate a leaked token by changing `.env` and restarting
`api`. Extending the edge rate-limiting rule of §5 to `/mcp` costs nothing, and a
Cloudflare Access policy in front of `/mcp` (service token) keeps unauthenticated
traffic off the origin entirely; the bearer check stays as the second layer.

A verdict an agent persists through a session is kept as history for that agent's own
thread and is **never** shown to Discord or web askers as a prior-call example: nobody
can rate it (there is no Discord message to vote on), and the answer text is the
outside agent's. Only the citations were validated.

A shell on the host can use the same tools without the network:
`docker compose run --rm --entrypoint judge-cli api card "Blood Moon"` (the `api` service's
entrypoint is `judge-api`, so `run` needs `--entrypoint`).

### One-time: upload the card-symbol emoji

The bot draws `{W}` as a picture using *application* emoji, which belong to the
Discord application rather than to any server. They survive redeploys and restores
(Discord stores them, not us), so this is once per application, not per deploy:

```sh
cargo run --release -p judge-ingest -- emoji   # needs DISCORD_TOKEN; no database
```

It is idempotent — it uploads only the symbols that are missing, so re-run it after
Scryfall adds one. Skipping it entirely is safe: the bot logs a warning at startup
and falls back to writing `{W}` as text. Nothing here needs doing for the web page,
which loads the symbols from Scryfall's CDN.

### Why `API_CLIENT_IP=cloudflare`, and not a forwarded-header flag

The per-IP limiter needs an address the caller cannot choose, because `/api/judge` is
anonymous and every request costs Anthropic tokens.

**`X-Forwarded-For` is not that address.** Cloudflare *appends* the connecting address
to a caller-supplied `X-Forwarded-For` rather than replacing it, so its first hop is
whatever the caller wrote. A client sending `X-Forwarded-For: 1.2.3.4` and incrementing
it per request would mint a fresh rate-limit allowance every time — through the tunnel,
which is the trusted path. Loopback binding does not help: the forged header rides in
over the tunnel like any other.

`CF-Connecting-IP` is set by Cloudflare on every request and cannot be forged by the
client, so that is what `API_CLIENT_IP=cloudflare` buckets on. `crates/api/src/http.rs`
never consults `X-Forwarded-For` at all, and `API_TRUST_FORWARDED` — which did — is now
rejected at startup rather than silently ignored.

Leave `API_CLIENT_IP=peer` for any deployment where Cloudflare is not the sole ingress:
`CF-Connecting-IP` is only trustworthy when nothing can reach the origin directly.

## 5. Protect the spend at the edge

With hosting at $0 the LLM bill is the entire bill, and the in-process limiter is the
backstop, not the front line.

- **Rate limiting rule** on `/api/judge`: match `API_RATE_LIMIT` /
  `API_RATE_WINDOW_SECS` (default 4 per 300s), or set it slightly tighter. The free
  plan includes one rule.
- **Managed Challenge** as a WAF custom rule on the HTML document request — *not* on
  `/api/judge`. A challenge served to an `XHR` cannot be solved by `fetch`, so
  challenging the API path breaks the page. Challenging the document gates a visitor
  once and subsequent `/api/judge` calls carry the `cf_clearance` cookie.

Full Turnstile with server-side `siteverify` is stronger, but needs a token in the
POST body and a verification call inside `judge_route` before any spending. Worth it
only if the edge rules prove insufficient.

## 6. Weekly backups to R2

Create an R2 bucket and an API token scoped to **Object Read & Write on that bucket
only**, fill in the `R2_*` values in `.env.deploy`, then install the cron entry:

```sh
crontab -e
15 4 * * 0  /path/to/mtg-judgebot/scripts/backup-db.sh >> ~/judgebot-backup.log 2>&1
```

Log to somewhere the running user can write — a `>>` into root-owned `/var/log` fails
before the script starts, leaving a backup that looks configured and never runs.

**On Synology DSM, do not use `crontab -e`** — DSM manages `/etc/crontab` in its own
format and can overwrite hand-edited user crontabs. Use **Control Panel → Task
Scheduler → Create → Scheduled Task → User-defined script**, set **User: root**
(Container Manager's Docker socket is root-only), and give it absolute paths, since
the scheduler runs with a minimal environment:

```sh
/volume1/homes/<you>/mtg-judgebot/scripts/backup-db.sh \
  >> /volume1/homes/<you>/judgebot-backup.log 2>&1
```

If `docker` isn't found, prefix the task with `PATH=/usr/local/bin:$PATH`. Tick the
task's email-on-error option so a failing backup is noisy rather than silent.

`scripts/backup-db.sh` dumps, gzips, refuses to upload anything under
`BACKUP_MIN_BYTES` (so a stub never becomes the newest restore point), uploads, and
only then prunes past `BACKUP_KEEP_DAYS`. Weekly runs at the default 60 days keep about
eight restore points, far inside R2's 10 GB free tier.

Run it once by hand to confirm credentials, then **do a restore drill** — an untested
backup is not a backup. The drill restores the object that actually landed in R2, not
a local copy:

```sh
scripts/backup-db.sh                       # take one
scripts/backup-db.sh list                  # newest last
scripts/backup-db.sh fetch judgebot-<stamp>.dump.gz

docker compose exec -T db createdb -U judgebot restoretest
gunzip -c judgebot-<stamp>.dump.gz \
  | docker compose exec -T db pg_restore -U judgebot -d restoretest
docker compose exec -T db psql -U judgebot -d restoretest -c "select count(*) from cards;"
docker compose exec -T db dropdb -U judgebot restoretest
rm judgebot-<stamp>.dump.gz
```

The card count should match section 2. `*.dump.gz` is gitignored.

## 7. Scheduled data refresh

Scryfall publishes new bulk data daily and Wizards ships a Comprehensive Rules
release with most sets. `judge-ingest refresh` brings the database up to date in one
unattended run, inside the same image `bot` and `api` run from (third entrypoint,
compose service `refresh`, off by default behind the `refresh` profile). It does, in
order: `cards` (Scryfall oracle cards, printed names, rulings — upserts, so cards the
bot already knows are refreshed in place), `rules latest` (reads Wizards' rules page,
compares the linked `MagicCompRules <date>.txt` against `max(rules.cr_version)` and
loads it only when the version differs), `retire` (re-checks every stored call's
citations against the data just loaded, see below), `embed` (only rows whose text changed — the CR
loader nulls the embedding of exactly those, so a new CR costs the embedder a few hundred
rules, not all of them) and `emoji` (uploads any card symbol Scryfall added; skipped
when `DISCORD_TOKEN` is unset). Each step runs even if an earlier one failed, and the
exit status is non-zero if any did.

With a `judge.toml`, `refresh` reads the same file `bot`/`api` do (compose mounts it
from `JUDGE_CONFIG`, §4): its `embed` step writes the vector space the bot queries, and
refuses when the two disagree.

`scripts/refresh-data.sh` is the cron entry point: it takes a lock so two runs never
overlap, then `docker compose run --rm --pull missing refresh`, which reuses the image
`docker compose pull` already fetched and never builds on the host. Any argument is
passed through as the `judge-ingest` subcommand, so `scripts/refresh-data.sh rules
latest` is a CR-only check.

Install it beside the backup, on the same scheduler (§6 has the Synology notes: run
as root, absolute paths, tick email-on-error):

```sh
crontab -e
30 5 * * *  /path/to/mtg-judgebot/scripts/refresh-data.sh >> ~/judgebot-refresh.log 2>&1
```

Daily is right for cards — Scryfall corrects Oracle text and adds rulings between
sets — and it bounds how long a new CR goes unnoticed to a day. Most days it costs one
Scryfall download and nothing else; the run takes a few minutes on a NAS, most of it
parsing the `default_cards` file, and does not disturb the running bot: every load is
one transaction, so retrieval sees the old data or the new, never a mix.

Prior calls follow the data they cite. The `retire` step asks of every stored call the
question that admitted it: does each cited rule, ruling or Oracle text still exist and
still contain the quote? A call that fails is retired (`calls.retired_at`, with the
offending citation in `retired_reason`) and leaves retrieval; one whose citations hold
again later is restored. So a CR release retires only the calls whose cited rules
actually changed, and an Oracle erratum retires the calls about that card (each call
remembers a fingerprint of its context cards' text), as a reworded ruling retires the
calls that quoted it. A rule that merely moved — Wizards inserts a keyword and the rest
of the section shifts by one — is followed: the `rules` step matches old and new rules
by text with rule numbers masked out, and rewrites the citations and answers of the
calls that cite it before the retirement check runs, so those calls stay live. Every
`rule renumbered` and `call relocated` is logged. To see what a run did:

```sql
select retired_reason, count(*) from calls where retired_at is not null group by 1;
```

The retriever's vector leg is blind to re-embedded rules for the minute between the
`rules` and `embed` steps; if `embed` fails (the embedder down, rate-limited) those rules stay
unembedded and the next night's run picks them up, since `embed` always fills every NULL.

Run it once by hand after installing, and expect the log to end with
`refresh step ok` five times. A one-off manual load still works the old way from a
workstation (`cargo run --release -p judge-ingest -- rules <url>`), which is also how
to force a re-parse of an already-loaded version: delete the cached txt first.

### Changing the embedding model (`reembed`)

Vectors from two models cannot share a column, so the database records which model's
vectors it holds (`embedding_space`, one row: provider kind, model, width) and every
reader and writer checks it first. A bot whose `[models.embed]` names a different model
or width does not mix: it logs `embedding space mismatch; vector legs off` and answers
from the curated map and full-text search alone until the two agree. Moving the database
to a new model is `judge-ingest reembed`, which pays the provider for every rule,
glossary entry and stored call again — the reason it is a dry run by default and the
reason to take a backup first.

```sh
scripts/backup-db.sh                          # a restore point holding the old vectors (§6)
$EDITOR judge.toml                            # [models.embed]: the new provider/model/dimensions
scripts/refresh-data.sh reembed               # dry run: what is stored, what would be cleared,
                                              # rows, a rough cost; probes the new model once;
                                              # exits non-zero having changed nothing
scripts/refresh-data.sh reembed --yes         # one transaction: retype vector(N), rebuild the
                                              # HNSW indexes, clear every vector, rewrite the
                                              # row — then the ordinary embed loop
docker compose restart bot api                # bot/api read judge.toml once, at startup;
                                              # `up -d` would see nothing to do (§4)
```

The running `bot`/`api` hold the `[models.embed]` they started with, so a restart is
required at some point; the space row itself they re-read on every request, so their
vector legs go dark the moment the row disagrees with their configuration and come back
the moment it agrees — dark, not mixed, in either order. Restarting before `--yes`
darkens them from the restart until the switch; restarting after darkens them from the
switch until the restart. Either way there is one dark window and no mixing; the order
above keeps it short. The refill is resumable: if the embed loop dies (rate limit, a
provider outage) `scripts/refresh-data.sh embed` — or the next nightly run — fills
whatever is still NULL, and retrieval degrades to the other legs for the rows not yet
embedded — and so does `reembed --yes` itself: with the row already switched it has
nothing to switch, so it fills the empty rows and pays for nothing twice. Clearing and
re-buying every vector in the same space takes `--clear`, deliberately; since the row
does not change, the running bot logs no mismatch and its vector leg simply answers
from nothing until the refill finishes. Resume outside
the nightly `refresh` window (the cron above): two refills at once each buy the same batch and one of them
then fails on rows the other already filled. If only the model *name* differs from
the row (same provider, same width) the dry run says so: when the stored vectors were
in fact produced by the configured model, `UPDATE embedding_space SET model = ...`
relabels them for nothing, and `--yes` would buy them all again. The
probe is what makes `--yes` safe to type: a wrong key, URL or model name, or a model
whose real width is not the configured `dimensions`, fails before anything is cleared,
because after the switch the only ways back are paying for the old space again or the
restore drill.

`reembed` needs the new `judge.toml` (`JUDGE_CONFIG` in `.env`) and the new provider's
`api_key_env` in `.env`; `refresh-data.sh` passes its arguments through to
`judge-ingest` inside the `refresh` container, which already has both. The dry run's
cost line is an order of magnitude at a generic list price, not a quote: ~$0.15 per
million tokens, chars-to-tokens at 4:1.

## 8. Redeploying

The host never builds. `.github/workflows/publish-image.yml` builds on every push to
`main` that touches the image (including `data/`, since `crates/core/build.rs`
generates the `Category` enum from `data/categories.yaml`) and pushes to
`ghcr.io/<owner>/<repo>` (the repository the workflow runs in) as `latest` plus an
immutable `sha-<short>` tag. The image is a manifest list for `linux/amd64` and
`linux/arm64`, each built on a runner of its own architecture, so an ARM host (a
Raspberry Pi, an ARM NAS, Apple silicon under Docker Desktop) pulls the same tag.
The compose file pulls `JUDGE_IMAGE`, which defaults to the upstream package; a fork
sets it to its own in `.env` once its first workflow run has published.

A GitHub release whose tag is `vX.Y.Z` adds version tags — `X.Y.Z`, `X.Y` and, from
1.0 on, `X` — to the image already built for that commit, without rebuilding it: the
release is the image that has been running as `latest`, down to the platform
digests. A host that
prefers to move on releases rather than on every push pins one:

```ini
JUDGE_IMAGE_TAG=0.3    # in .env: follows 0.3.x patch releases; 0.3.1 pins one exactly
```

`CONTRIBUTING.md` says how a release is cut.

```sh
git pull                                # runbook + compose changes
docker compose pull
docker compose up -d
```

`docker compose up -d --build` still works on a machine with the CPU and RAM for it;
`build: .` is retained for local development.

### A release that carries a migration

`bot` and `api` apply pending migrations at startup, before anything else touches
the database, so the ordinary deploy above is complete for a release whose commit
adds a file under `crates/bot/migrations/`: the first of the two to start migrates
(the calls-rewrite advisory lock serialises them; sqlx's own migrator lock is a
second layer for a concurrent `sqlx migrate run`), the other finds nothing pending,
and the log says `schema migrated` with the versions. A migration that fails exits the process,
which under `restart: unless-stopped` is a crash-loop with the reason in
`docker compose logs bot` — loud on purpose, where a bot running against the wrong
schema would answer questions and quietly fail to persist them.

The migration holds the same advisory lock the refresh job's CR load, retirement
pass and embedding writes take, so those wait for it and it waits for them (the
Scryfall card/rulings upsert takes no lock and needs none: one transaction, no
`calls` rows). A deploy that lands during the nightly refresh therefore sits at
startup until the CR load finishes — minutes on a NAS — with one warning line,
`another job holds the calls rewrite lock ... waiting`, in `docker compose logs bot`;
that is a wait, not a hang. It does not stop the *other* service: compose recreates `bot` and `api`
independently, so for a migration that is not additive — one that rewrites `calls`
rows (20260902000001 did, moving ruling citations to content keys) must not race a
call being persisted by the old binary, and `ALTER TABLE` waits on any in-flight
query — stop both first. The release notes in the commit say when this applies:

```sh
git pull
docker compose pull
docker compose stop bot api      # only when the release notes say the migration rewrites rows
docker compose up -d
```

To move the schema by hand instead, set `JUDGE_AUTO_MIGRATE=false` in `.env` (it
reaches `bot`/`api` at their next recreate, which `up -d` does because the env file
changed) and run the explicit form from the *new* image:

```sh
docker compose pull
docker compose stop bot api                              # only when the migration rewrites rows
docker compose run --rm --pull missing refresh migrate   # prints what it applies; refuses a changed file
docker compose up -d
```

### One-time: let the host pull a private package

The upstream package is public and needs no login. A fork's package inherits the
fork's visibility, so a private fork's host must log in with a classic PAT carrying
only `read:packages` — as root on Synology, since that is the user Container Manager
and the Task Scheduler run as:

```sh
echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-user> --password-stdin
```

Making the package public instead (GHCR package settings, independent of repo
visibility) removes the login step.

### Rolling back

Every build leaves an immutable tag, so a bad deploy is a one-line revert. Take the
`sha-<short>` from the workflow run summary, or the version of the last good release:

```ini
JUDGE_IMAGE_TAG=sha-abc1234    # in .env; or a release, e.g. 0.3.1
```

```sh
docker compose pull && docker compose up -d
```

Clear `JUDGE_IMAGE_TAG` to return to `latest`.

An older tag starting against a newer schema logs `database is ahead of this binary`
and does not migrate; whether it then works depends on the migration. An additive one
(a new table, a nullable column) is harmless to the old binary. One that changed a
column the old queries use is not, and rolling back across it means restoring the
pre-release dump too (`scripts/backup-db.sh fetch`, §6), which is why the weekly
backup is worth taking by hand right before such a deploy. `judge-ingest migrate`
refuses an ahead database outright, rather than guessing.

`cloudflared` and `db` are untouched by a code deploy. The tunnel reconnects on its
own if the connector restarts.

## 9. Troubleshooting

| Symptom | Cause |
| --- | --- |
| 502 from the public hostname | `api` is down, or the tunnel's service is not `http://api:8787` |
| Tunnel healthy, hostname NXDOMAIN | the `judge` record is grey-clouded; it must be proxied |
| `/mcp` answers 401 | wrong or missing `Authorization: Bearer <MCP_TOKEN>` |
| `/mcp` answers 403 while `/api/health` is fine | the public hostname is not in `MCP_ALLOWED_HOSTS` |
| `/mcp` answers 405 (a browser GET shows the web page) | `MCP_TOKEN` is unset in the api container's `.env`, so `/mcp` is just another page path |
| Everyone shares one rate-limit bucket | `API_CLIENT_IP=peer` behind the tunnel — every request looks like the cloudflared container |
| Rate limiting never triggers | `API_CLIENT_IP=cloudflare` while something other than Cloudflare can reach the origin, so `CF-Connecting-IP` is caller-supplied |
| `judge-api` exits citing `API_TRUST_FORWARDED` | that variable was removed as unsafe; use `API_CLIENT_IP` |
| Bot online, web page dead | expected if only `api` failed — the gateway is a separate outbound connection |
| `cloudflared` restart-loops on startup | `COMPOSE_PROFILES=tunnel` with `TUNNEL_TOKEN` empty or stale in `.env.deploy` |
| Backup cron silently never runs | log path not writable by your user, or `.env.deploy` missing |
| Refresh exits `another refresh is running` with nothing running | a previous run was killed before removing `.refresh.lock` in the repo root; `rmdir` it |
| Refresh loads the CR every night | `rules.cr_version` disagrees with the file name on Wizards' page — check the `current comprehensive rules release` log line for `published` vs `stored` |
| Refresh runs but the bot still cites the old CR | it does not: retrieval reads the database live; check the run actually finished (`refresh step ok` for `rules` and `embed`) |
| `JUDGE_CONFIG=/etc/judgebot/judge.toml: file not found` at startup | `JUDGE_CONFIG` in `.env` names a host file that does not exist; Docker mounted an empty directory in its place (and created a root-owned one on the host — `sudo rmdir` it) |
| `providers.X: NAME (api_key_env) is not set` at startup | the key was exported in the shell that ran `cargo run` but never written to `.env`, which is all the containers read; or, from `refresh` alone, only the embed provider's key was set because "refresh only embeds" — `refresh` resolves the chat stages too, so the extract/synth providers' keys must be in `.env` as well |
| Edited `judge.toml`, `docker compose up -d`, nothing changed | `up -d` recreates only on a configuration or image change and a bind-mounted file's content is neither; `docker compose restart bot api` (§4) |
| `providers.X (...): no credentials` at startup | a cloud door with an empty chain: no `AWS_*` in `.env`, no mounted credentials file, or a mounted file the `nobody` user cannot read |
| `embedding space mismatch; vector legs off` | `[models.embed]` names a model or width other than the one the database holds; `reembed` (§7) to move the data, or change the file back |
| `no price for X/Y` at startup | a model on an `openai` provider without `[models.<stage>.pricing]`; add one (USD per million tokens) or `pricing = "free"` on the provider |
