# Deployment

The bot and API run on a machine you own, behind a Cloudflare Tunnel. There is no
public IP, no forwarded port and no cloud compute bill. `cloudflared` dials *out* to
Cloudflare's edge and traffic returns down that connection.

```
Browser ──https──> Cloudflare edge        [TLS, WAF, rate limiting, challenge]
                        ↕ outbound tunnel — no inbound port on the host
                   cloudflared ──http──> api:8787 ──> db:5432
                                         bot ──outbound WS──> Discord
```

This keeps two properties that a serverless split would lose:

- The Discord gateway stays a long-lived connection, with no HTTP-interactions rewrite.
- The spend cap, concurrency semaphore and rate limiter stay in-memory values in one
  process rather than becoming distributed state. A budget period adds one small table
  beside the cap, not a service.

See `docs/ARCHITECTURE.md` for the pipeline itself.

Hostnames, paths and the image name below are placeholders (`judge.example.com`,
`/path/to/mtg-judgebot`, `ghcr.io/<owner>/<repo>`). Substitute your own.

## 1. Prerequisites

- A host that stays on, with Docker and the compose plugin. The stack *runs* in about
  200 MB RSS (Postgres ~157 MB, api and bot a few MB each), so 2 GB of RAM is ample.
  The host never *builds*: CI publishes the image and the host pulls it (§8). A build
  (`cargo build --release` across ten crates plus a Vite build) wants ~4 GB and a CPU
  that a NAS does not have.
- The compose file uses only syntax that older bundled Compose versions accept.
  Synology's Container Manager ships v2.20, which predates the `env_file` long form.
  As a result, `.env.deploy` must exist on any machine running the `tunnel` profile,
  and should exist only there.
- No Rust toolchain is needed on the host. The image carries its own migrations, and
  `bot` and `api` apply pending ones at startup (`JUDGE_AUTO_MIGRATE`, on by default).
  `docker compose run --rm refresh migrate` is the explicit form, for an empty
  database or an operator who opted out. Restoring a dump (§2) brings the schema
  *and* the `_sqlx_migrations` ledger with it.
- A domain whose DNS is hosted **on Cloudflare**. Tunnel hostnames resolve only for
  records in the same Cloudflare account, so a third-party DNS host cannot CNAME to
  `<uuid>.cfargotunnel.com`. On the free plan that means moving the whole zone.

## 2. Data migration

This section is for moving an instance that already has data to another host.
**A new instance has nothing to migrate.** Skip to §3, and load the data in §4 with
`docker compose run --rm refresh init` (the documentation site's "Requirements and first
run" page is the walkthrough).

When moving, do this before anything else. Restore a dump rather than re-ingesting. A
cold rebuild re-parses the CR and the Scryfall bulk file and pays the embedding provider
again for every rule and glossary entry. Stored calls and ratings cannot be re-ingested at
all: they are lost.

```sh
# old host
docker exec judgebot-db pg_dump -U judgebot -Fc judgebot > judgebot.dump

# new host
docker compose up -d db
docker exec -i judgebot-db pg_restore -U judgebot -d judgebot --clean --if-exists \
  < judgebot.dump
```

Check the restore before moving on:

```sh
docker compose exec -T db psql -U judgebot -d judgebot \
  -c "select (select count(*) from cards) cards,
             (select count(*) from rulings) rulings,
             (select count(*) from rules where embedding is not null) embedded,
             (select provider||'/'||model||'/'||dimensions from embedding_space) space;"
```

If `embedded` is 0, the vector leg is off. Run `scripts/refresh-data.sh embed`
rather than shipping a degraded retriever.

The vector leg is also off when `space` is empty or names a model other than the one
the bot is configured with. The bot's vector legs stay off until the row and the
configuration agree. It logs an error-level line at startup and when the row changes,
and it never mixes vectors from two models in one column. The fix depends on the cause:

- The row is mislabelled: `UPDATE embedding_space SET model = ...`.
- The model is changing: `scripts/refresh-data.sh reembed --yes`. This costs money,
  because it embeds every row again.
- The row is already right: the same command only fills whatever is still empty.

## 3. Create the tunnel

In Cloudflare **Zero Trust → Networks → Tunnels**, create a tunnel (remotely managed)
and add a public hostname:

| Field | Value |
| --- | --- |
| Subdomain | `judge` |
| Domain | `example.com` |
| Service | `http://api:8787` |

`api` is the compose service name. cloudflared resolves it on the compose network, so
the API never needs a published port. Cloudflare creates the
`judge CNAME <uuid>.cfargotunnel.com` record for you, proxied. It must stay **proxied**
(orange cloud), unlike every other record in the zone.

Copy the connector token into `.env.deploy` as `TUNNEL_TOKEN`.

## 4. Configure and start

Configuration lives in two separate files:

```sh
cp .env.example .env               # app config: API keys, DISCORD_TOKEN, GUILD_ID,
                                   # JUDGE_OPERATOR_DISCORD and JUDGE_OPERATOR_EMAIL (both required here)
cp .env.deploy.example .env.deploy # deploy credentials: TUNNEL_TOKEN, R2_*
```

Both files are gitignored.

- `.env` is the `env_file` for `bot`, `api` and `refresh`.
- `.env.deploy` is read only by `cloudflared` and `scripts/backup-db.sh`. A token that
  can rewrite the tunnel or delete every backup therefore never reaches the
  internet-facing API.

Model credentials belong in `.env`: `ANTHROPIC_API_KEY`, every `api_key_env` a
`judge.toml` names, and the cloud doors' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS`.
`bot`, `api` and `refresh` read them, and nothing else does.

In `.env`, set:

```ini
COMPOSE_PROFILES=tunnel     # `docker compose up -d` now includes cloudflared
API_CLIENT_IP=cloudflare    # rate-limit on CF-Connecting-IP
JUDGE_MAX_USD=...           # the backstop for anonymous traffic
JUDGE_BUDGET_PERIOD=month   # one budget for bot and api, kept across restarts
JUDGE_ALERT_WEBHOOK=...     # told when the cap trips, or a refresh or backup fails
```

Set a budget period on a host that runs unattended. Without `JUDGE_BUDGET_PERIOD` the
cap is per process and per lifetime: `bot` and `api` can each spend `JUDGE_MAX_USD`,
and every restart (a redeploy, a crash loop) resets them to zero. `judge-cli stats` shows
what each day cost.

If `COMPOSE_PROFILES` in `.env` doesn't take effect on an older Compose, pass
`--profile tunnel` instead.

Then bring it up. A restored database (§2) already carries the schema and the
migration ledger, so there is nothing to migrate:

```sh
docker compose up -d
curl -s localhost:8787/api/health
```

An *empty* database also needs only `docker compose up -d`: `bot` and `api` create the
schema at startup. Use the explicit form below to see what is about to be applied, or
when `JUDGE_AUTO_MIGRATE=false`. It runs from the same image and needs nothing but
Docker (`run` starts `db` if it is not up):

```sh
docker compose pull
docker compose run --rm --pull missing refresh migrate   # never falls back to building on the host
```

You can also run `sqlx migrate run --source crates/bot/migrations` from a workstation,
over an SSH tunnel to the loopback-bound Postgres. It writes the same ledger with the
same checksums. It skips the refresh-job lock and the "database is ahead" check, so
prefer the container form on a live host.

Use a full `docker compose up -d` whenever the compose file's `db` service changes.
`up -d --build bot api` leaves `db` alone, so it would keep the old port binding or
healthcheck.

### Other model providers

This step is optional. Without a `judge.toml` the containers use Anthropic directly
with `ANTHROPIC_API_KEY`, and Voyage with `VOYAGE_API_KEY`, as set in `.env`. A
`judge.toml` runs the judge on other providers: a cheap model for extraction, Claude
through your own cloud account, a local Ollama, an OpenAI-compatible gateway.
`judge.example.toml` documents every knob, and the README's "Choosing a model" is the
short version. Write the file and name it in `.env`:

```ini
JUDGE_CONFIG=./judge.toml      # a host path: what `cargo run` reads, and what compose mounts
```

`docker-compose.yml` bind-mounts that file read-only into `bot`, `api` and `refresh` at
`/etc/judgebot/judge.toml` and points the containers' `JUDGE_CONFIG` there. One
variable serves both the host and the containers. When it is blank, compose mounts the
tracked `judge.example.toml` only so that the mount has a source. Nothing reads it, and
the setup stays the `.env` one. Three consequences:

- **The `api_key_env` of every provider a stage names must be set in `.env`**, including
  for `refresh`. Each binary resolves all three stages (`[models.extract]`,
  `[models.synth]`, `[models.embed]`) at load. A missing chat key therefore fails
  `refresh`'s nightly `embed` step, even though `refresh` never uses it. That is deliberate:
  it fails rather than run half a configuration. A provider
  table no stage names is parsed but its key is never read. `.env` is the `env_file` for
  all three containers, so one line there covers them.
- **The cloud doors take credentials from the platform chain, not the file.** For
  `claude-platform-on-aws` and `bedrock`, either put `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`) in `.env`, or mount a credentials
  file and point to it. The containers run as `nobody` with no home directory, so the
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

  For `vertex`, do the same with a service-account JSON and
  `GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp/sa.json`. An IAM role that can only invoke
  the model is enough, because nothing here manages infrastructure. `refresh` needs none
  of this: embeddings are Voyage or OpenAI-compatible, never a cloud door. None of it
  goes in `.env.deploy`, which `bot`/`api` do not read.
- **The startup log tells you what resolved.**
  - Every binary logs one `config=... extract=... synth=... embed=... cap=$...` line.
  - `bot`, `api`, `eval` and `judge-cli` make chat calls, so they then log
    `cloud credentials resolved` per cloud provider. The chain is probed once at
    startup, so a host with no credentials exits there, naming the provider and the
    door. `refresh` makes no chat call and never probes.
  - Those four binaries then log `embedding space matches the database` or
    `embedding space mismatch` (below).
  - `docker compose run --rm --entrypoint judge-cli api config` prints the resolution
    as JSON, secrets redacted.

After editing `judge.toml`, run `docker compose restart bot api`, not `up -d`. The file
is read only at startup. Compose recreates a container only when its configuration or
image changed, and a bind-mounted file's content is neither, so `up -d` prints `Running`
and keeps the old configuration. (Changing `JUDGE_CONFIG` itself in `.env` does change
the configuration, and `up -d` recreates.) The next `refresh` run picks the
file up on its own. Changing `[models.embed]` is the one edit that needs the database
moved too (§7).

An older Compose may reject the `${JUDGE_CONFIG:+…}` interpolation in
`docker-compose.yml` at parse time. If so, set the containers' side by hand in the same
override file: `environment: {JUDGE_CONFIG: /etc/judgebot/judge.toml}` on `bot`, `api`
and `refresh`, with the mount left as it is.

### MCP endpoint

This step is optional. `judge-api` can serve the judge's tools (`crates/agent`) to an
MCP client over the same tunnel, at `/mcp`. It needs both the `--mcp` interface and an
`MCP_TOKEN`:

- `--mcp` without a token is refused at startup.
- A token without `--mcp` logs a startup warning, so a token in `.env` is never
  mistaken for a live endpoint.

There is no anonymous mode. A request without `Authorization: Bearer <MCP_TOKEN>` gets
a 401 before the protocol sees it. Behind the token are:

- `judge`: the full pipeline with model spend, under the same `JUDGE_MAX_USD` and
  `JUDGE_CONCURRENCY` as the web page.
- The agent-driven sessions: no model calls, database work only.
- The read-only lookups.

```ini
API_INTERFACES=--api --web --mcp          # the api container's front doors; without --mcp the token only warns
MCP_TOKEN=<openssl rand -base64 32>       # at least 24 characters, or the API refuses to start
MCP_ALLOWED_HOSTS=judge.example.com,localhost   # Host values accepted: the tunnel's hostname, plus
                                                   # localhost for curl on the host; the list replaces the default
```

Behind the tunnel, `MCP_ALLOWED_HOSTS` must name the public hostname. The MCP transport accepts only the `Host`
values on its list, which defaults to loopback (a guard against DNS rebinding), and
cloudflared forwards the public hostname. Without it, every `/mcp` request gets a 403
while `/api/judge` keeps working. Then run `docker compose up -d api` and, from a workstation:

```sh
claude mcp add --transport http judge https://judge.example.com/mcp   --header "Authorization: Bearer <MCP_TOKEN>"
```

The per-IP rate limit of `/api/judge` does not apply to `/mcp`, where the token is the
identity. Instead, `judge` runs through `/mcp` are capped per window
(`MCP_JUDGE_LIMIT` per `MCP_JUDGE_WINDOW_SECS`, default 20 an hour). The shared
`JUDGE_CONCURRENCY` slots and `JUDGE_MAX_USD` cap apply as well. This window limits
what a leaked token can spend: about `MCP_JUDGE_LIMIT × $0.10` an hour, and no faster.

Sessions and lookups make no chat-model call. They do embed the question or search
text with the configured embedder, if there is one. That costs fractions of a cent, and
it is the only paid upstream call with no cap.

To rotate a leaked token, change it in `.env` and restart `api`. Two optional extra
layers:

- Extend the edge rate-limiting rule of §5 to `/mcp`. It costs nothing.
- Put a Cloudflare Access policy (service token) in front of `/mcp`. It keeps
  unauthenticated traffic off the origin, and the bearer check stays as a second layer.

A verdict an agent persists through a session is kept only as history for that agent's
own thread. It is **never** shown to Discord or web askers as a prior-call example.
Nobody can rate it, because there is no Discord message to vote on. The answer text is
the outside agent's, and only its citations were validated.

A shell on the host can use the same tools without the network:
`docker compose run --rm --entrypoint judge-cli api card "Blood Moon"`. The `api`
service's entrypoint is `judge-api`, so `run` needs `--entrypoint`.

### Card-symbol emoji

The bot draws `{W}` as a picture using *application* emoji. These belong to the
Discord application rather than to any server, and Discord stores them, so they
survive redeploys and restores. Upload them once per application, not once per deploy:

```sh
cargo run --release -p judge-ingest -- emoji   # needs DISCORD_TOKEN; no database
```

The command is idempotent. It uploads only the symbols that are missing, so re-run it
after Scryfall adds one. Skipping it is safe: the bot logs a warning at startup
and falls back to writing `{W}` as text. The web page needs none of this, because it
loads the symbols from Scryfall's CDN.

### Client address for rate limiting

The per-IP limiter needs an address the caller cannot choose, because `/api/judge` is
anonymous and every request costs Anthropic tokens.

**`X-Forwarded-For` is not that address.** Cloudflare *appends* the connecting address
to a caller-supplied `X-Forwarded-For` rather than replacing it, so its first hop is
whatever the caller wrote. A client could send `X-Forwarded-For: 1.2.3.4`, increment it
on each request, and get a fresh rate-limit allowance every time. Binding to loopback
does not help, because the forged header arrives through the tunnel, the trusted path.

Cloudflare sets `CF-Connecting-IP` on every request and the client cannot forge it.
`API_CLIENT_IP=cloudflare` buckets on that header. `crates/api/src/http.rs` never reads
`X-Forwarded-For`.

Leave `API_CLIENT_IP=peer` for any deployment where Cloudflare is not the sole ingress.
`CF-Connecting-IP` is trustworthy only when nothing can reach the origin directly.

### Behind another reverse proxy

The tunnel is a choice, not a requirement. `api` publishes on `127.0.0.1:8787`, and any
reverse proxy on the host can terminate TLS in front of it. Two things need care.

**Answers take up to a minute**, so the proxy's read timeout must be longer than that.
Caddy's default is unlimited. nginx's is 60 seconds.

**Every request now arrives from the proxy**, so `API_CLIENT_IP=peer` puts all visitors in
one rate-limit bucket. To restore the per-address limit, have the proxy set
`CF-Connecting-IP` to the client's address, overwriting whatever the client sent, and
set `API_CLIENT_IP=cloudflare`. This is sound for the same reason as behind Cloudflare:
something you control sets the header, and the origin listens on loopback only, so
nothing off the host can reach it. It holds only while that proxy is the public edge.
With Cloudflare in front of the proxy as well, `{remote_host}` is Cloudflare's address,
so use the tunnel setup instead.

```caddyfile
judge.example.com {
    reverse_proxy 127.0.0.1:8787 {
        header_up CF-Connecting-IP {remote_host}
    }
}
```

```nginx
server {
    server_name judge.example.com;          # plus your TLS configuration
    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header CF-Connecting-IP $remote_addr;
        proxy_read_timeout 120s;
    }
}
```

If the proxy does not set that header, leave `API_CLIENT_IP=peer` and rate limit at the
proxy instead (nginx's `limit_req`), because the in-process limit then counts everyone
together. With `--mcp`, add the public hostname to `MCP_ALLOWED_HOSTS` as in the tunnel
setup. §5's edge rules are Cloudflare features, so without Cloudflare only the
in-process limit, the concurrency slots and the spend budget protect the model bill
from anonymous traffic.

## 5. Edge spend protection

With hosting at $0, the LLM bill is the entire bill. The edge rules below are the
first line of defence, and the in-process limiter is the backstop.

- **Rate limiting rule** on `/api/judge`: match `API_RATE_LIMIT` /
  `API_RATE_WINDOW_SECS` (default 4 per 300s), or set it slightly tighter. The free
  plan includes one rule.
- **Managed Challenge** as a WAF custom rule on the HTML document request, *not* on
  `/api/judge`. `fetch` cannot solve a challenge served to an `XHR`, so challenging the
  API path breaks the page. Challenging the document checks a visitor once, and later
  `/api/judge` calls carry the `cf_clearance` cookie.

Full Turnstile with server-side `siteverify` is stronger. It needs a token in the POST
body and a verification call inside `judge_route` before any spending. Add it only if
the edge rules prove insufficient.

With a budget period set, a capped instance comes back by itself when the period turns.
To resume sooner, raise `JUDGE_MAX_USD` and `docker compose up -d`. The period's spend is
in the database, so the restart does not reset it.

## 6. Weekly backups to R2

1. Create an R2 bucket.
2. Create an API token scoped to **Object Read & Write on that bucket only**.
3. Fill in the `R2_*` values in `.env.deploy`.
4. Install the cron entry:

```sh
crontab -e
15 4 * * 0  /path/to/mtg-judgebot/scripts/backup-db.sh >> ~/judgebot-backup.log 2>&1
```

Log to a path the running user can write. A `>>` into root-owned `/var/log` fails
before the script starts, so the backup looks configured but never runs.

**On Synology DSM, do not use `crontab -e`.** DSM manages `/etc/crontab` in its own
format and can overwrite hand-edited user crontabs. Use **Control Panel → Task
Scheduler → Create → Scheduled Task → User-defined script** and set **User: root**
(Container Manager's Docker socket is root-only). Use absolute paths, because
the scheduler runs with a minimal environment:

```sh
/volume1/homes/<you>/mtg-judgebot/scripts/backup-db.sh \
  >> /volume1/homes/<you>/judgebot-backup.log 2>&1
```

If `docker` isn't found, prefix the task with `PATH=/usr/local/bin:$PATH`. Tick the
task's email-on-error option so a failing backup is noisy rather than silent.

`scripts/backup-db.sh` works in this order:

1. It dumps and gzips the database.
2. It refuses to upload anything under `BACKUP_MIN_BYTES`, so a stub never becomes the
   newest restore point.
3. It uploads.
4. Only then does it prune past `BACKUP_KEEP_DAYS`.

Weekly runs at the default 60 days keep about eight restore points, far inside R2's
10 GB free tier.

Run it once by hand to confirm credentials, then **do a restore drill**. An untested
backup is not a backup. The drill restores the object that landed in R2, not
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

`judge-ingest refresh` brings the database up to date in one unattended run. Scryfall
publishes new bulk data daily, and Wizards ships a Comprehensive Rules release with
most sets. The job uses the same image as `bot` and `api`, with a third entrypoint: the
compose service `refresh`, off by default behind the `refresh` profile. Its steps, in
order:

1. `cards`: Scryfall oracle cards, printed names and rulings. These are upserts, so
   cards the bot already knows are refreshed in place.
2. `rules latest`: reads Wizards' rules page, compares the linked
   `MagicCompRules <date>.txt` against `max(rules.cr_version)` and loads it only when
   the version differs.
3. `retire`: re-checks every stored call's citations against the data just loaded (see
   below).
4. `embed`: only rows whose text changed. The CR loader nulls the embedding of those
   rows alone, so a new CR costs the embedder a few hundred rules, not all of them.
5. `emoji`: uploads any card symbol Scryfall added. Skipped when `DISCORD_TOKEN` is
   unset.

Each step runs even if an earlier one failed, and the exit status is non-zero if any
did.

With a `judge.toml`, `refresh` reads the same file `bot`/`api` do (compose mounts it
from `JUDGE_CONFIG`, §4). Its `embed` step writes vectors in the space the bot queries,
and refuses when the configured space and the database's disagree.

`scripts/refresh-data.sh` is the cron entry point. It takes a lock so two runs never
overlap, then runs `docker compose run --rm --pull missing refresh`. That reuses the
image `docker compose pull` already fetched and never builds on the host. Any argument
is passed through as the `judge-ingest` subcommand. For example,
`scripts/refresh-data.sh rules latest` checks only the CR.

Install it beside the backup, on the same scheduler. §6 has the Synology notes: run
as root, absolute paths, tick email-on-error.

```sh
crontab -e
30 5 * * *  /path/to/mtg-judgebot/scripts/refresh-data.sh >> ~/judgebot-refresh.log 2>&1
```

Run it daily. Scryfall corrects Oracle text and adds rulings between sets, and a new
CR is then picked up within a day. Most days it costs one Scryfall download and nothing
else. The run takes a few minutes on a NAS, mostly parsing the `default_cards` file. It does not disturb the running bot: every
load is one transaction, so retrieval sees the old data or the new, never a mix.

Prior calls follow the data they cite. The `retire` step re-runs the check that
admitted each stored call: does each cited rule, ruling or Oracle text still exist and
still contain the quote? A call that fails is retired (`calls.retired_at`, with the
failing citation in `retired_reason`) and drops out of retrieval. A call whose
citations hold again later is restored. The effects:

- A CR release retires only the calls whose cited rules changed.
- An Oracle erratum retires the calls about that card, because each call remembers a
  fingerprint of its context cards' text.
- A reworded ruling retires the calls that quoted it.

Calls follow a rule that was only renumbered, as when Wizards inserts a keyword and
the rest of the section shifts by one. The `rules` step matches old and new rules by
text, with rule numbers masked out. Before the retirement check runs, it rewrites the
citations and answers of the calls that cite the rule, so those calls stay live.
Every `rule renumbered` and `call relocated` is logged. To see what a run did:

```sql
select retired_reason, count(*) from calls where retired_at is not null group by 1;
```

Between the `rules` and `embed` steps (about a minute), the vector leg cannot find
the changed rules. If `embed` fails (the embedder is down or rate-limited), those rules
stay unembedded until the next night's run, because `embed` always fills every NULL.

Run it once by hand after installing, and expect the log to end with
`refresh step ok` five times. A one-off manual load also works from a
workstation (`cargo run --release -p judge-ingest -- rules <url>`). To force a
re-parse of an already-loaded version that way, delete the cached txt first.

### Embedding model change

Vectors from two models cannot share a column. The database therefore records which
model's vectors it holds (`embedding_space`, one row: provider kind, model, width), and
every reader and writer checks it first. If a bot's `[models.embed]` names a different
model or width, it does not mix them. It logs `embedding space mismatch; vector legs off` and
answers from the curated map and full-text search alone until the two agree.

`judge-ingest reembed` moves the database to a new model. It pays the provider to
embed every rule, glossary entry and stored call again. So it is a dry run by default,
and you should take a backup first.

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

The running `bot`/`api` keep the `[models.embed]` they started with, so they must be
restarted at some point. They re-read the space row on every request. Their vector legs
turn off as soon as the row disagrees with their configuration and back on as soon as
it agrees. Either order works:

- Restarting before `--yes` turns them off from the restart until the switch.
- Restarting after `--yes` turns them off from the switch until the restart.

Either way there is one window without vectors, and no mixing. The order above keeps it short.

The refill is resumable. If the embed loop dies (rate limit, a provider outage),
`scripts/refresh-data.sh embed` or the next nightly run fills whatever is still NULL.
Retrieval degrades to the other legs for the rows not yet embedded. `reembed --yes`
resumes too: with the row already switched it has nothing to switch, so it fills the
empty rows and pays for nothing twice.

`--clear` clears and re-buys every vector in the same space. The row does not change,
so the running bot logs no mismatch, and its vector leg finds nothing until the refill
finishes.

Resume outside the nightly `refresh` window (the cron above). Two refills at once both
pay for the same batch, and one then fails on rows the other already filled.

If only the model *name* differs from the row (same provider, same width), the dry run
says so. If the configured model did produce the stored vectors,
`UPDATE embedding_space SET model = ...` relabels them for free, while `--yes` would
pay for them all again.

`--yes` probes the new model before it clears anything. A wrong key, URL or model
name, or a model whose width is not the configured `dimensions`, fails at that point.
This matters because after the switch the only ways back are paying for the old space
again or restoring a backup (the restore drill, §6).

`reembed` needs the new `judge.toml` (`JUDGE_CONFIG` in `.env`) and the new provider's
`api_key_env` in `.env`. `refresh-data.sh` passes its arguments through to
`judge-ingest` inside the `refresh` container, which already has both. The dry run's
cost line is a rough order of magnitude, not a quote. It assumes a generic list price
of ~$0.15 per million tokens and 4 characters per token.

## 8. Redeploying

The host never builds. `.github/workflows/publish-image.yml` builds on every push to
`main` that touches the image. That includes `data/`, because `crates/core/build.rs`
generates the `Category` enum from `data/categories.yaml`. The workflow pushes to
`ghcr.io/<owner>/<repo>` (the repository it runs in) with two tags: `latest` and an
immutable `sha-<short>`. The image covers both `linux/amd64` and `linux/arm64` (a
manifest list, each built on a runner of its own architecture). An ARM host (a
Raspberry Pi, an ARM NAS, Apple silicon under Docker Desktop) pulls the same tag.

The compose file pulls `JUDGE_IMAGE`, which defaults to the upstream package. A fork
sets it in `.env` to its own package once its first workflow run has published. A fork
also sets `JUDGE_SOURCE_URL` to its repository. Every interface's source offer (the web
footer, `/help` and `/license`, the MCP instructions) then points users at the code that
is running. The AGPL asks this of anyone serving a modified version.

The workflow stamps the image with the commit it built, so the offer names the
revision. It does this with `JUDGE_COMMIT`, a Docker build argument the Dockerfile
hands to `crates/bot/build.rs`. A local `docker compose build` has no `.git` in its
context, so it must pass the commit and whether the tree matches it. Otherwise the offer
reads "commit unknown":

```sh
JUDGE_COMMIT=$(git rev-parse HEAD) JUDGE_DIRTY=$(git diff-index --quiet HEAD || echo 1) \
  docker compose up -d --build bot api
```

A `JUDGE_COMMIT` that is not a commit id (a branch name, a tag) fails the build rather
than becoming "commit unknown" on every interface.

A GitHub release tagged `vX.Y.Z` adds version tags to the image already built for that
commit, without rebuilding it: `X.Y.Z`, `X.Y` and, from 1.0 on, `X`. The release is
exactly the image that has been running as `latest`, down to the platform digests. To
update on releases rather than on every push, pin one:

```ini
JUDGE_IMAGE_TAG=1.0    # in .env: follows 1.0.x patch releases; 1.0.0 pins one exactly
```

`CONTRIBUTING.md` says how a release is cut. The ordinary deploy:

```sh
git pull                                # runbook + compose changes
docker compose pull
docker compose up -d
```

`docker compose up -d --build` still works on a machine with the CPU and RAM for it.
The compose file keeps `build: .` for local development.

### Releases with a migration

The ordinary deploy above is complete for a release that adds a file under
`crates/bot/migrations/`. `bot` and `api` apply pending migrations at startup, before
anything else touches the database. The first of the two to start migrates, and the
other finds nothing pending. The calls-rewrite advisory lock keeps them from running at
once, and sqlx's own migrator lock is a second layer against a concurrent
`sqlx migrate run`. The log says `schema migrated` with the versions.

A failed migration exits the process. Under `restart: unless-stopped` that is a
crash-loop, with the reason in `docker compose logs bot`. This is loud on purpose: a
bot running against the wrong schema would answer questions and quietly fail to save
them.

The refresh job's CR load, retirement pass and embedding writes take the same advisory
lock, so the migration and those steps wait for each other. (The Scryfall card/rulings
upsert takes no lock and needs none: it is one transaction and touches no `calls`
rows.) A deploy during the nightly refresh therefore waits at startup until the CR load
finishes, which is minutes on a NAS. It logs one warning line,
`another job holds the calls rewrite lock ... waiting`, in `docker compose logs bot`.
That is a wait, not a hang.

The lock does not stop the *other* service, because compose recreates `bot` and `api`
independently. So stop both first for a migration that is not additive. A migration
that rewrites `calls` rows (20260902000001 did, moving ruling citations to content
keys) must not race the old binary saving a call, and `ALTER TABLE` waits on any
in-flight query. The release notes in the commit say when this applies:

```sh
git pull
docker compose pull
docker compose stop bot api      # only when the release notes say the migration rewrites rows
docker compose up -d
```

To migrate by hand instead, set `JUDGE_AUTO_MIGRATE=false` in `.env`. `bot`/`api`
pick it up when next recreated, which `up -d` does because the env file changed. Then run the explicit form from the *new* image:

```sh
docker compose pull
docker compose stop bot api                              # only when the migration rewrites rows
docker compose run --rm --pull missing refresh migrate   # prints what it applies; refuses a changed file
docker compose up -d
```

### Private package pulls

The upstream package is public and needs no login. A fork's package has the
fork's visibility, so a private fork's host must log in once with a classic PAT that has
only `read:packages`. On Synology, log in as root, because Container Manager and the
Task Scheduler run as root:

```sh
echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-user> --password-stdin
```

Making the package public instead (GHCR package settings, independent of repo
visibility) removes the login step.

### Rolling back

Every build leaves an immutable tag, so reverting a bad deploy is a one-line change. Take the
`sha-<short>` from the workflow run summary, or the version of the last good release:

```ini
JUDGE_IMAGE_TAG=sha-abc1234    # in .env; or a release, e.g. 1.0.0
```

```sh
docker compose pull && docker compose up -d
```

Clear `JUDGE_IMAGE_TAG` to return to `latest`.

An older tag starting against a newer schema logs `database is ahead of this binary`
and does not migrate. Whether it then works depends on the migration:

- An additive one (a new table, a nullable column) is harmless to the old binary.
- One that changed a column the old queries use is not. Rolling back across it means
  restoring the pre-release dump too (`scripts/backup-db.sh fetch`, §6). Run the
  weekly backup by hand right before such a deploy.

`judge-ingest migrate` refuses a database that is ahead rather than guessing.

`cloudflared` and `db` are untouched by a code deploy. The tunnel reconnects on its
own if the connector restarts.

## 9. Troubleshooting

| Symptom | Cause |
| --- | --- |
| 502 from the public hostname | `api` is down, or the tunnel's service is not `http://api:8787` |
| Tunnel healthy, hostname NXDOMAIN | the `judge` record is grey-clouded; it must be proxied |
| `/mcp` answers 401 | wrong or missing `Authorization: Bearer <MCP_TOKEN>` |
| `/mcp` answers 403 while `/api/health` is fine | the public hostname is not in `MCP_ALLOWED_HOSTS` |
| `/mcp` answers 405 (a browser GET shows the web page) | `--mcp` is not in `API_INTERFACES`, so `/mcp` is just another page path |
| `api` exits naming `--mcp` and `MCP_TOKEN` | `--mcp` with no token to gate it; set `MCP_TOKEN` or drop the flag |
| `/mcp` 404s and the log warns about `MCP_TOKEN` | the token is set but `--mcp` is not in `API_INTERFACES` |
| `api` exits naming `--web` and `index.html` | `--web` with no built page at `WEB_DIST`; drop `--web` or rebuild the image |
| The page 404s but `/api/health` is fine | `--web` is not in `API_INTERFACES`; the startup log line lists what is on and what is off |
| Everyone shares one rate-limit bucket | `API_CLIENT_IP=peer` behind the tunnel — every request looks like the cloudflared container |
| Rate limiting never triggers | `API_CLIENT_IP=cloudflare` while something other than Cloudflare can reach the origin, so `CF-Connecting-IP` is caller-supplied |
| `bot` or `api` restart-loops naming `JUDGE_OPERATOR_DISCORD` / `JUDGE_OPERATOR_EMAIL` | the contact that surface must show is unset or malformed in `.env`; set it and `docker compose up -d` |
| Bot online, web page dead | expected if only `api` failed — the gateway is a separate outbound connection |
| `cloudflared` restart-loops on startup | `COMPOSE_PROFILES=tunnel` with `TUNNEL_TOKEN` empty or stale in `.env.deploy` |
| Members are told the bot "hit its spending cap" | `JUDGE_MAX_USD` is spent for the process or the period (`judge-cli stats` shows the days); raise it and `docker compose up -d`, or wait for the period to turn |
| A refresh or backup failed and nobody noticed | set `JUDGE_ALERT_WEBHOOK` in `.env`; both scripts post there on a non-zero exit |
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
