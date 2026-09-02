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

Live deployment: <https://mtgjudge.rpeters.dev>

## 1. Prerequisites

- A host that stays on, with Docker and the compose plugin. The stack *runs* in about
  200 MB RSS (Postgres ~157 MB, api and bot a few MB each), so 2 GB of RAM is ample.
  It never has to *build*: CI publishes the image and the host pulls it (§8). That
  matters because `cargo build --release` across seven crates plus a Vite build wants
  ~4 GB and real CPU, which a NAS does not have.
- Compose syntax here is held to what older bundled versions accept — Synology's
  Container Manager ships v2.20, which predates the `env_file` long form. `.env.deploy`
  must exist on any machine running the `tunnel` profile, and only there.
- No Rust toolchain on the host. Restoring a dump (§2) brings the schema *and* the
  `_sqlx_migrations` ledger with it, so `sqlx migrate run` is only needed for a fresh
  empty database or after pulling new migrations — and it can be run from a
  workstation over an SSH tunnel rather than installed on the server.
- A domain whose DNS is hosted **on Cloudflare**. Tunnel hostnames resolve only for
  records in the same Cloudflare account, so third-party DNS cannot CNAME to
  `<uuid>.cfargotunnel.com`; the free plan requires moving the whole zone.

## 2. Move the data (do this before anything else)

Restore a dump rather than re-ingesting. A cold rebuild re-parses the CR and the
Scryfall bulk file and re-embeds every rule through Voyage, which costs money.

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
             (select count(*) from rules where embedding is not null) embedded;"
```

`embedded` being 0 means the vector leg is silently off — re-run
`judge-ingest -- embed` rather than shipping a degraded retriever.

## 3. Create the tunnel

In Cloudflare **Zero Trust → Networks → Tunnels**, create a tunnel (remotely managed)
and add a public hostname:

| Field | Value |
| --- | --- |
| Subdomain | `mtgjudge` |
| Domain | `rpeters.dev` |
| Service | `http://api:8787` |

`api` is the compose service name — cloudflared resolves it on the compose network, so
the API never needs a published port. Cloudflare creates the proxied
`mtgjudge CNAME <uuid>.cfargotunnel.com` record for you; it must stay **proxied**
(orange cloud), unlike every other record in the zone.

Copy the connector token into `.env.deploy` as `TUNNEL_TOKEN`.

## 4. Configure and start

Two files, deliberately separate:

```sh
cp .env.example .env               # app config: API keys, DISCORD_TOKEN, GUILD_ID
cp .env.deploy.example .env.deploy # deploy credentials: TUNNEL_TOKEN, R2_*
```

`.env` is the `env_file` for `bot` and `api`. `.env.deploy` is read only by
`cloudflared` and `scripts/backup-db.sh`, so a token that can rewrite the tunnel or
delete every backup never enters the environment of the internet-facing API. Both are
gitignored.

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

Starting from an *empty* database instead, run migrations before `bot` and `api` or
they crash-loop against a missing schema. From a workstation with `sqlx-cli`, over an
SSH tunnel to the server's loopback-bound Postgres:

```sh
ssh -N -L 5433:127.0.0.1:5433 you@server &
sqlx migrate run --source crates/bot/migrations   # DATABASE_URL=...@localhost:5433
```

Use a full `docker compose up -d` at least once on an existing host: `db`'s published
port changed to loopback, and `up -d --build bot api` deliberately leaves `db` alone,
so the old `0.0.0.0:5433` binding would otherwise persist indefinitely.

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
/volume1/homes/ryan/mtg-judgebot/scripts/backup-db.sh \
  >> /volume1/homes/ryan/judgebot-backup.log 2>&1
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
loader nulls the embedding of exactly those, so a new CR costs Voyage a few hundred
rules, not all of them) and `emoji` (uploads any card symbol Scryfall added; skipped
when `DISCORD_TOKEN` is unset). Each step runs even if an earlier one failed, and the
exit status is non-zero if any did.

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
`rules` and `embed` steps; if `embed` fails (Voyage down, rate-limited) those rules stay
unembedded and the next night's run picks them up, since `embed` always fills every NULL.

Run it once by hand after installing, and expect the log to end with
`refresh step ok` five times. A one-off manual load still works the old way from a
workstation (`cargo run --release -p judge-ingest -- rules <url>`), which is also how
to force a re-parse of an already-loaded version: delete the cached txt first.

## 8. Redeploying

The host never builds. `.github/workflows/publish-image.yml` builds on every push to
`main` that touches the image (including `data/`, since `crates/core/build.rs`
generates the `Category` enum from `data/categories.yaml`) and pushes to
`ghcr.io/sloshy/mtg-judgebot` as `latest` plus an immutable `sha-<short>` tag.

```sh
git pull                                # runbook + compose changes
docker compose pull
docker compose up -d
```

`docker compose up -d --build` still works on a machine with the CPU and RAM for it;
`build: .` is retained for local development.

### A release that carries a migration

Neither `bot` nor `api` runs migrations at startup, so a release whose commit adds a
file under `crates/bot/migrations/` needs the schema moved by hand, with the old
binaries stopped first: the new image's queries fail against the old schema (every
question that resolves a card errors) and the old image's fail against the new one.
Nothing crash-loops, so the failure is quiet until someone asks a question.

```sh
git pull
docker compose stop bot api
ssh -N -L 5433:127.0.0.1:5433 you@server &     # from a workstation with sqlx-cli
sqlx migrate run --source crates/bot/migrations  # DATABASE_URL=...@localhost:5433
docker compose pull && docker compose up -d
```

Stopping `bot`/`api` matters for more than the error window: a migration that
rewrites `calls` rows (20260902000001 did, moving ruling citations to content keys)
must not race a call being persisted by the old binary, and `ALTER TABLE` waits on
any in-flight query. The release notes in the commit say when this applies; the
refresh cron is harmless meanwhile, since a failing step rolls back.

### One-time: let the host pull a private package

The repo is private, so the GHCR package is too. On the host, log in with a classic
PAT carrying only `read:packages` — as root on Synology, since that is the user
Container Manager and the Task Scheduler run as:

```sh
echo "$GHCR_TOKEN" | docker login ghcr.io -u sloshy --password-stdin
```

Making the package public instead (GHCR package settings, independent of repo
visibility) removes the login step but publishes the built binaries.

### Rolling back

Every build leaves an immutable tag, so a bad deploy is a one-line revert. Take the
`sha-<short>` from the workflow run summary:

```ini
JUDGE_IMAGE_TAG=sha-abc1234    # in .env
```

```sh
docker compose pull && docker compose up -d
```

Clear `JUDGE_IMAGE_TAG` to return to `latest`.

A tag from before a migration cannot run against the migrated schema. Rolling back
across one means restoring the pre-release dump too (`scripts/backup-db.sh fetch`, §6),
which is why the weekly backup is worth taking by hand right before such a deploy.

`cloudflared` and `db` are untouched by a code deploy. The tunnel reconnects on its
own if the connector restarts.

## 9. Troubleshooting

| Symptom | Cause |
| --- | --- |
| 502 from the public hostname | `api` is down, or the tunnel's service is not `http://api:8787` |
| Tunnel healthy, hostname NXDOMAIN | the `mtgjudge` record is grey-clouded; it must be proxied |
| Everyone shares one rate-limit bucket | `API_CLIENT_IP=peer` behind the tunnel — every request looks like the cloudflared container |
| Rate limiting never triggers | `API_CLIENT_IP=cloudflare` while something other than Cloudflare can reach the origin, so `CF-Connecting-IP` is caller-supplied |
| `judge-api` exits citing `API_TRUST_FORWARDED` | that variable was removed as unsafe; use `API_CLIENT_IP` |
| Bot online, web page dead | expected if only `api` failed — the gateway is a separate outbound connection |
| `cloudflared` restart-loops on startup | `COMPOSE_PROFILES=tunnel` with `TUNNEL_TOKEN` empty or stale in `.env.deploy` |
| Backup cron silently never runs | log path not writable by your user, or `.env.deploy` missing |
| Refresh exits `another refresh is running` with nothing running | a previous run was killed before removing `.refresh.lock` in the repo root; `rmdir` it |
| Refresh loads the CR every night | `rules.cr_version` disagrees with the file name on Wizards' page — check the `current comprehensive rules release` log line for `published` vs `stored` |
| Refresh runs but the bot still cites the old CR | it does not: retrieval reads the database live; check the run actually finished (`refresh step ok` for `rules` and `embed`) |
