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

- A host that stays on, with Docker and the compose plugin. Compose syntax here
  is kept to what older bundled versions accept (Synology's Container Manager in
  particular lags); `.env.deploy` must exist on any machine running the `tunnel`
  profile, and only there. The stack idles at
  roughly 200 MB RSS (Postgres ~157 MB, api and bot a few MB each), so 2 GB of RAM is
  ample. Building the image locally wants ~4 GB and real CPU — on a small ARM box,
  build elsewhere and pull instead.
- A git checkout plus `sqlx-cli` (`cargo install sqlx-cli`) to run migrations. That
  is the only step needing a Rust toolchain on the host; everything else is Docker.
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

Then bring it up — migrations first, or `bot` and `api` crash-loop against an empty
schema until they run:

```sh
docker compose up -d db
~/.cargo/bin/sqlx migrate run --source crates/bot/migrations
docker compose up -d
curl -s localhost:8787/api/health
```

Use a full `docker compose up -d` at least once on an existing host: `db`'s published
port changed to loopback, and `up -d --build bot api` deliberately leaves `db` alone,
so the old `0.0.0.0:5433` binding would otherwise persist indefinitely.

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

Log to somewhere your own user can write — `crontab -e` edits *your* crontab, and a
`>>` into root-owned `/var/log` fails before the script starts, leaving a backup that
looks configured and never runs.

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

## 7. Redeploying

```sh
git pull
docker compose up -d --build bot api    # one image, two entrypoints
```

`cloudflared` and `db` are untouched by a code deploy. The tunnel reconnects on its
own if the connector restarts.

## 8. Troubleshooting

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
