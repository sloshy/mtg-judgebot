# Deployment

judgebot runs on a machine you own, with Docker Compose: Postgres, the `judgebot`
process and a small Caddy proxy in front of it. Out of the box it answers on
`127.0.0.1:8787` on that machine and nowhere else. That is all a trial, or a Discord bot
over its gateway connection, needs. To put the web app, the API or Discord's
interactions endpoint on the internet, choose a public route (§3): the bundled HTTPS site, a
reverse proxy of your own, or a Cloudflare Tunnel.

```
Browser ──https──> host :443 ──> edge (Caddy, Let's Encrypt) ──> judgebot:8787 ──> db:5432
                                 [COMPOSE_PROFILES=public]       judgebot ──outbound WS──> Discord
Browser on the host ──> 127.0.0.1:8787 ──> proxy ──> judgebot:8787
Operator (loopback or SSH) ──> 127.0.0.1:8789 ──> proxy:8789 ──> judgebot:8789   [--admin]
```

With a Cloudflare Tunnel instead, `cloudflared` dials *out* to Cloudflare and the
public hostname reaches `proxy:8787` over the compose network, with no inbound port.

One host keeps two properties that a serverless split would lose:

- The Discord bot can keep a long-lived gateway connection (the default) rather than
  needing a public endpoint ([Discord over HTTP](#discord-over-http) is the option).
- The spend cap, concurrency semaphore and rate limiter stay in-memory values in one
  process rather than becoming distributed state. A budget period adds one small table
  beside the cap, not a service.

See `docs/ARCHITECTURE.md` for the pipeline itself.

Hostnames, paths and the image name below are placeholders (`judge.example.com`,
`/path/to/mtg-judgebot`, `ghcr.io/<owner>/<repo>`). Substitute your own.

## 1. Prerequisites

- A host that stays on, with Docker and the compose plugin. The stack *runs* in about
  200 MB RSS (Postgres ~157 MB, judgebot a few MB), so 2 GB of RAM is ample.
  The host never *builds*: CI publishes the image and the host pulls it (§8). A build
  (`cargo build --release` across ten crates plus a Vite build) wants ~4 GB and a CPU
  that a NAS does not have.
- The compose file uses only syntax that older bundled Compose versions accept.
  Synology's Container Manager ships v2.20, which predates the `env_file` long form.
  As a result, `.env.deploy` must exist on any machine running the `tunnel` or
  `backup` profile, and should exist only there.
- No Rust toolchain is needed on the host. The image carries its own migrations, and
  `judgebot` applies pending ones at startup (`JUDGE_AUTO_MIGRATE`, on by default).
  `docker compose run --rm refresh migrate` is the explicit form, for an empty
  database or an operator who opted out. Restoring a dump (§2) brings the schema
  *and* the `_sqlx_migrations` ledger with it.
- For a public address, a domain name, and one of:
  - for the bundled HTTPS site, a DNS record for it pointing at the host, and ports 80
    and 443 reachable from the internet (forwarded by the router at home);
  - for a Cloudflare Tunnel, the domain's DNS hosted **on Cloudflare**. Tunnel
    hostnames resolve only for records in the same Cloudflare account, so a third-party
    DNS host cannot CNAME to `<uuid>.cfargotunnel.com`. On the free plan that means
    moving the whole zone.

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

If `embedded` is 0, the vector search is off. Run `scripts/refresh-data.sh embed`
rather than shipping a degraded retriever.

The vector search is also off when `space` is empty or names a model other than the one
the bot is configured with. The bot's vector search stays off until the row and the
configuration agree. It logs an error-level line at startup and when the row changes,
and it never mixes vectors from two models in one column. The fix depends on the cause:

- The row is mislabelled: `UPDATE embedding_space SET model = ...`.
- The model is changing: `scripts/refresh-data.sh reembed --yes`. This costs money,
  because it embeds every row again.
- The row is already right: the same command only fills whatever is still empty.

## 3. Exposure

Compose publishes nothing beyond the host's loopback until a profile in
`COMPOSE_PROFILES` says so. Choose one route:

| Route | For | In `.env` |
| --- | --- | --- |
| [Local only](#local-only) | a trial, or Discord over its gateway connection | nothing |
| [Public HTTPS](#public-https) | a host that can take ports 80 and 443 | `COMPOSE_PROFILES=public`, `JUDGE_DOMAIN`, `API_CLIENT_IP=proxy` |
| [Own reverse proxy](#own-reverse-proxy) | a host that already runs one | `API_CLIENT_IP=proxy` |
| [Cloudflare Tunnel](#cloudflare-tunnel) | no inbound port or public IP, or Cloudflare's rules in front | `COMPOSE_PROFILES=tunnel`, `API_CLIENT_IP=cloudflare` |

Whichever it is, the process's own limits guard the model bill (§5). The tunnel adds
Cloudflare's rules in front of them.

### Local only

Nothing to set. `docker compose up -d` starts `db`, `judgebot` and `proxy`, and the web
app and the API answer at `http://127.0.0.1:8787` on the host. A Discord bot over its
gateway connection (the default) dials out to Discord and needs no inbound connection at
all. From another machine, forward the port over SSH:
`ssh -N -L 8787:127.0.0.1:8787 <host>`.

On a trusted network the proxy can publish the port on the host's LAN address as well,
through a `docker-compose.override.yml` as for the admin interface (§4):

```yaml
services:
  proxy:
    ports:
      - "192.168.1.50:8787:8787" # the host's LAN address : the port to open : 8787
```

The container side must be 8787. The host's own `127.0.0.1:8787` maps to the
container's 8788, the site that trusts a reverse proxy on the host (Client address,
below), and nothing else may: never map another host address to 8788.

The LAN then reaches the proxy's 8787 site, which passes `CF-Connecting-IP` through.
Under `API_CLIENT_IP=cloudflare`, anyone on that network can send one and choose their
own rate-limit bucket, so keep `peer` (or `proxy`, whose header that site removes) on a
host that publishes 8787 beyond loopback.

### Public HTTPS

The `edge` service (the `public` profile) is a second Caddy, on ports 80 and 443 of
every address of the host. It serves `https://<JUDGE_DOMAIN>` with a certificate it gets
from Let's Encrypt and renews itself, and redirects plain HTTP there. It forwards to
`judgebot:8787` the way `proxy` does, so a deploy (§8) hands over without a gap and
never restarts it.

1. Point a DNS `A` record for the name at the host's public IPv4 address. Behind a
   router, forward TCP ports 80 and 443 to the host (and UDP 443, for HTTP/3). A home
   connection whose address changes needs a dynamic DNS client to keep the record
   current. The record must not be proxied by Cloudflare (orange cloud): every visitor
   would then arrive from a Cloudflare address, one rate-limit bucket per Cloudflare
   server under `API_CLIENT_IP=proxy`. Keep it DNS only (grey cloud), or use the
   tunnel instead.

   An `AAAA` record for the host's IPv6 address is optional, and costs the per-visitor
   limit for IPv6 unless the compose network has IPv6 too. Without it, Docker passes an
   IPv6 connection on through its userland proxy, so `edge` sees the network's gateway
   as the address of every IPv6 visitor, and they all share one bucket. Either leave
   the `AAAA` record out, or enable IPv6 on the compose network in a
   `docker-compose.override.yml` (Docker 27 or later gives it a range of its own):

   ```yaml
   networks:
     default:
       enable_ipv6: true
   ```

   then `docker compose down && docker compose up -d`, since a network is not changed
   in place. Each visitor's address then reaches `edge`, and the limit counts an IPv6
   visitor by its /64 prefix, the block one subscriber is given, so picking another
   address in it buys no fresh allowance.
2. Set in `.env` (§4):

   ```ini
   COMPOSE_PROFILES=public          # `docker compose up -d` now includes edge
   JUDGE_DOMAIN=judge.example.com   # the name alone: no scheme, port, path or trailing dot
   API_CLIENT_IP=proxy              # rate-limit on the address edge saw (Client address)
   ```

3. `docker compose up -d`, then `curl -s https://judge.example.com/api/health`. The
   first request waits a few seconds for the certificate; `docker compose logs edge`
   shows Caddy getting it.

What to know:

- `edge` refuses to start without `JUDGE_DOMAIN`, or with one that is not a host name,
  and its log names the variable. A host name here is two or more dot-separated labels
  of letters, digits and inner hyphens (1 to 63 characters each, 253 in all), the last
  not all digits, so not `localhost` or an IP address, with no scheme, port, path or
  trailing dot. The config editor checks the same rule.
- Let's Encrypt must reach the host under that name on port 80 or 443. While the record
  or the forwarding is wrong, Caddy retries with a growing pause. Let's Encrypt limits
  failed attempts per hour, so fix the cause rather than restarting `edge` repeatedly.
- The certificates live in the `judgebot-edge-data` volume, so a restart or an upgrade
  does not ask for them again.
- Its configuration is `proxy/edge.Caddyfile`, mounted and watched like the proxy's.
  `scripts/deploy.sh` checks that it loads before a deploy, and starts `edge` when the
  profile is on.
- Answers take up to a minute, and Caddy's proxy has no read timeout by default. When
  `edge` or `proxy` is stopped or recreated, it finishes the requests in flight for up
  to 70 s (`grace_period`), inside compose's `stop_grace_period` of 80 s.
- Docker must hand `edge` the visitor's address. Rootless Docker and Docker Desktop
  pass published ports through a proxy of their own, so every visitor arrives from one
  private address and shares one bucket; run the stack on rootful Docker Engine for a
  per-visitor limit.
- Something else on the host holding port 80 or 443 (a web server, a NAS's own page)
  keeps `edge` from starting. Use your own reverse proxy then.
- `MCP_ALLOWED_HOSTS` names `JUDGE_DOMAIN` (§4, MCP endpoint), and Discord's
  Interactions Endpoint URL is `https://<JUDGE_DOMAIN>/discord/interactions` (§4,
  Discord over HTTP).

### Own reverse proxy

A reverse proxy already running on the host can terminate TLS in front of
`127.0.0.1:8787`, the `proxy` service. That address passes the `Host` header through
unchanged, and a deploy does not restart it, so it stays up while `judgebot` is replaced.
A proxy in a container of its own reaches the host's loopback only with
`network_mode: host`. Point nothing on the compose network at `proxy:8788`, and never
map a host address other than `127.0.0.1` to it: that site passes the client headers
through, so whatever reaches it chooses its own bucket. Two things need care.

**Answers take up to a minute**, so the proxy's read timeout must be longer than that.
Caddy's default is unlimited. nginx's is 60 seconds.

**Every request arrives from the proxy**, so `API_CLIENT_IP=peer` puts all visitors in
one rate-limit bucket. To restore the per-address limit, have the proxy set
`X-Judgebot-Client-IP` to the client's address, replacing any copy the client sent, and
set `API_CLIENT_IP=proxy`. This is sound because something you control sets the header
and `127.0.0.1:8787` is reachable from the host alone. It holds only while that proxy is
the public edge. With Cloudflare in front of the proxy as well, `{remote_host}` is
Cloudflare's address, so use the tunnel setup instead.

```caddyfile
judge.example.com {
    reverse_proxy 127.0.0.1:8787 {
        header_up X-Judgebot-Client-IP {remote_host}
    }
}
```

```nginx
server {
    server_name judge.example.com;          # plus your TLS configuration
    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header X-Judgebot-Client-IP $remote_addr;
        proxy_read_timeout 120s;
    }
}
```

A proxy that *adds* the header instead of setting it (HAProxy's `http-request
add-header`) leaves the client's copy beside its own, and judgebot then trusts neither
and counts the proxy's address. If the proxy cannot set it, leave `API_CLIENT_IP=peer`
and rate limit at the proxy instead (nginx's `limit_req`). With `--mcp`, add the public
hostname to `MCP_ALLOWED_HOSTS`.

**A proxy on another machine** (a router's or a NAS's reverse proxy on the LAN) cannot
use `proxy` mode: it would need 8787 published on the LAN, whose site removes the
header (§3, Local only). Keep `API_CLIENT_IP=peer` and rate limit at that proxy.

### Cloudflare Tunnel

In Cloudflare **Zero Trust → Networks → Tunnels**, create a tunnel (remotely managed)
and add a public hostname:

| Field | Value |
| --- | --- |
| Subdomain | `judge` |
| Domain | `example.com` |
| Service | `http://proxy:8787` |

`proxy` is the compose service in front of `judgebot`'s HTTP listener. cloudflared
resolves it on the compose network, so nothing needs a published port. It forwards
each request to whichever `judgebot` container is listening, so requests keep being
answered while a deploy, which runs two for a minute, switches between them. It also
answers to `api`, the name of the service that served HTTP before `bot` and `api`
became one, so a tunnel set up then with `http://api:8787` keeps working. A tunnel whose
service is `http://judgebot:8787` reaches the containers directly: a deploy can lose a
request in flight, and a caller's `X-Judgebot-Client-IP` is not removed on that path, so
change it to `http://proxy:8787`. Cloudflare creates the
`judge CNAME <uuid>.cfargotunnel.com` record for you, proxied. It must stay
**proxied** (orange cloud), unlike every other record in the zone.

Copy the connector token into `.env.deploy` as `TUNNEL_TOKEN`, and set in `.env` (§4):

```ini
COMPOSE_PROFILES=tunnel     # `docker compose up -d` now includes cloudflared
API_CLIENT_IP=cloudflare    # rate-limit on CF-Connecting-IP
```

Cloudflare's rules in front of the origin are in §5.

### Client address

The per-address limiter needs an address the caller cannot choose, because
`/api/judge` is anonymous and every request costs model tokens. `API_CLIENT_IP` says
where it comes from:

| Value | Buckets on | Sound while |
| --- | --- | --- |
| `peer` (default) | the connection's address | nothing sits in front, or every visitor may share one bucket |
| `proxy` | `X-Judgebot-Client-IP` | the bundled site, or your own proxy, is the only public route |
| `cloudflare` | `CF-Connecting-IP` | Cloudflare is the only public route |

**`X-Forwarded-For` is not that address.** Cloudflare *appends* the connecting address
to a caller-supplied `X-Forwarded-For` rather than replacing it, so its first hop is
whatever the caller wrote. A client could send `X-Forwarded-For: 1.2.3.4`, increment it
on each request, and get a fresh rate-limit allowance every time. Binding to loopback
does not help, because the forged header arrives through the tunnel, the trusted path.
`crates/api/src/http.rs` never reads it.

**`proxy`.** `edge` sets `X-Judgebot-Client-IP` to the address it was dialled from,
overwriting any copy the caller sent. The proxy's other sites remove it:
`proxy:8787`, where a tunnel's hostname points, and the admin site. The proxy's
`:8788` site passes it on, for a reverse proxy on the host: the host's `127.0.0.1:8787`
maps to it, and any container on the compose network can dial `proxy:8788` as well, so
nothing on that network may be pointed there. `judgebot` publishes no port, so what
remains is another container on the compose network dialling `judgebot:8787` or
`proxy:8788` itself, such as a tunnel whose service names either. As a last check,
`judgebot` reads the header only from a peer on a loopback or private address and only
when exactly one copy arrived; otherwise it counts the peer. That check keeps out a
caller connecting from a public address of its own, nothing more: where Docker rewrites
the source, every caller is a private address.

The limit counts an IPv4 address by itself and an IPv6 address by its /64 prefix.
Docker hands `edge` the visitor's IPv4 address. An IPv6 visitor reaches it through
Docker's userland proxy unless the compose network has IPv6 enabled, and then arrives
from the network's gateway, so IPv6 visitors share one bucket (Public HTTPS, step 1,
has the two ways out). Rootless Docker and Docker Desktop do the same for every
visitor. Both err on the side of counting too much, never too little.

**`cloudflare`.** Cloudflare sets `CF-Connecting-IP` on every request and the client
cannot forge it. `CF-Connecting-IP` is trustworthy only when nothing can reach the
origin except through Cloudflare. `edge` removes a caller's copy, so the `public` and
`tunnel` profiles can run side by side: under `cloudflare` the visitors through `edge`
share one bucket, and under `proxy` those through the tunnel do.

## 4. Configure and start

Configuration lives in two separate files:

```sh
docker compose pull                # the image, which also carries the config editor
scripts/config.sh                  # app config (.env) in the config editor: API keys, DISCORD_TOKEN,
                                   # GUILD_ID, JUDGE_OPERATOR_DISCORD and JUDGE_OPERATOR_EMAIL (both
                                   # required here), and judge.toml. Over SSH, forward the port first:
                                   # ssh -N -L 8790:127.0.0.1:8790 <host>
cp .env.deploy.example .env.deploy # with the tunnel or backup profile: TUNNEL_TOKEN, R2_*
```

The editor saves `.env`, creating it from `.env.example` the first time, and checks each
change with the binaries' loaders. To edit by hand instead, `cp .env.example .env` and fill
it in. `.env.deploy` is edited by hand: the editor never reads it.

Both files are gitignored.

- `.env` is the `env_file` for `judgebot` and `refresh`.
- `.env.deploy` is read only by `cloudflared`, the `backup` service and
  `scripts/backup-db.sh`. A token that can rewrite the tunnel or delete every backup
  therefore never reaches the internet-facing API. The `backup` service in turn reads
  nothing of `.env`.

Model credentials belong in `.env`: `ANTHROPIC_API_KEY`, every `api_key_env` a
`judge.toml` names, and the cloud endpoints' `AWS_*`/`GOOGLE_APPLICATION_CREDENTIALS`.
`judgebot` and `refresh` read them, and nothing else does.

In the editor's Settings tab (or `.env`), set:

```ini
COMPOSE_PROFILES=public     # the route from §3 (public or tunnel; empty for local only),
                            # plus backup for the weekly backup (§6): public,backup
JUDGE_DOMAIN=...            # with public: the host name it serves (§3)
API_CLIENT_IP=proxy         # proxy with public or your own proxy, cloudflare with the tunnel
JUDGE_MAX_USD=...           # the backstop for anonymous traffic (§5)
JUDGE_BUDGET_PERIOD=month   # one budget for the month, kept across restarts
JUDGE_SPEND_WARN_AT=50,80   # warn at these percentages of the cap first
JUDGE_ALERT_WEBHOOK=...     # told when the cap trips, a refresh or backup fails, or Discord's setup is off
                            # (the backup service reads it from .env.deploy, §6)
```

Set a budget period on a host that runs unattended. Without `JUDGE_BUDGET_PERIOD` the
cap is per process and per lifetime: every restart (a redeploy, a crash loop) resets it
to zero. `judge-cli stats` shows
what each day cost.

If `COMPOSE_PROFILES` in `.env` doesn't take effect on an older Compose, pass
`--profile public` (or `--profile tunnel`) instead.

Then bring it up. A restored database (§2) already carries the schema and the
migration ledger, so there is nothing to migrate:

```sh
docker compose up -d
curl -s localhost:8787/api/health
```

Port 8787 on the host is the `proxy` service, which forwards to `judgebot`. The
`judgebot` container publishes nothing itself, so that a deploy can run two of them
side by side (§8).

The container's healthcheck asks `/api/health` the same question, and `cloudflared`
waits for it to pass. It probes only when the roles include `--api`, `--web` or
`--mcp`, or `--discord` with `DISCORD_INTERACTIONS=http`: it reads them from the
process's command line, or from `JUDGE_ROLES` when the command line names none. With
no HTTP role over the gateway (`JUDGE_ROLES='--discord --jobs'`) nothing listens, so the check passes without probing rather than marking the container
unhealthy forever. The Discord gateway is not visible from outside the process; a
fatal disconnect exits it, and `restart: unless-stopped` brings it back. A process
standing by for the gateway (§8, Gateway holder) is healthy: it is waiting by design.

An *empty* database also needs only `docker compose up -d`: `judgebot` creates the
schema at startup. Use the explicit form below to see what is about to be applied, or
when `JUDGE_AUTO_MIGRATE=false`. It runs from the same image and needs nothing but
Docker (`run` starts `db` if it is not up):

```sh
docker compose pull
docker compose run --rm refresh migrate   # the image `pull` fetched; build only if it is missing, so pull first
```

You can also run `sqlx migrate run --source crates/bot/migrations` from a workstation,
over an SSH tunnel to the loopback-bound Postgres. It writes the same ledger with the
same checksums. It skips the refresh-job lock and the "database is ahead" check, and it
records no release in `instance_release`, so a `judge-cli` or `judge-mcp` stops against
the result until `judgebot` next migrates. Prefer the container form on a live host.

Use a full `docker compose up -d` whenever the compose file's `db` service changes.
`up -d --build judgebot` leaves `db` alone, so it would keep the old port binding or
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

`docker-compose.yml` bind-mounts that file read-only into `judgebot` and `refresh` at
`/etc/judgebot/judge.toml` and points the containers' `JUDGE_CONFIG` there. One
variable serves both the host and the containers. When it is blank, compose mounts the
tracked `judge.example.toml` only so that the mount has a source. Nothing reads it, and
the setup stays the `.env` one. Three consequences:

- **The `api_key_env` of every provider a stage names must be set in `.env`**, including
  for `refresh`. Each binary resolves all three stages (`[models.extract]`,
  `[models.synth]`, `[models.embed]`) at load. A missing chat key therefore fails
  `refresh`'s `embed` step, even though `refresh` never uses it. That is deliberate:
  it fails rather than run half a configuration. A provider
  table no stage names is parsed but its key is never read. `.env` is the `env_file` for
  both services, so one line there covers them.
- **The cloud endpoints take credentials from the platform chain, not the file.** For
  `claude-platform-on-aws` and `bedrock`, either put `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`) in `.env`, or mount a credentials
  file and point to it. The containers run as `nobody` with no home directory, so the
  default `~/.aws` location does not exist:

  ```yaml
  # docker-compose.override.yml (gitignored like judge.toml; compose merges it in by itself)
  services:
    judgebot:
      volumes: ["/home/you/.aws:/etc/aws:ro"]
      environment:
        AWS_SHARED_CREDENTIALS_FILE: /etc/aws/credentials
        AWS_CONFIG_FILE: /etc/aws/config
        AWS_PROFILE: judgebot
  ```

  For `vertex`, do the same with a service-account JSON and
  `GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp/sa.json`. An IAM role that can only invoke
  the model is enough, because nothing here manages infrastructure. `refresh` needs none
  of this: embeddings are Voyage or OpenAI-compatible, never a cloud endpoint. None of it
  goes in `.env.deploy`, which `judgebot` does not read.
- **The startup log tells you what resolved.**
  - Every binary logs one `config=... extract=... synth=... embed=... cap=$...` line.
  - `judgebot` (with a serving role), `eval` and `judge-cli` make chat calls, so they then log
    `cloud credentials resolved` per cloud provider. The chain is probed once at
    startup, so a host with no credentials exits there, naming the provider and the
    endpoint. `refresh` makes no chat call and never probes.
  - Those three binaries then log `embedding space matches the database` or
    `embedding space mismatch` (below).
  - `docker compose run --rm --entrypoint judge-cli judgebot config` prints the resolution
    as JSON, secrets redacted.
  - `judge-config` edits `judge.toml` and `.env` from a page on localhost and checks each
    draft with the same loader (site: The config editor, for the `docker compose run`
    line that runs it here).

After editing `judge.toml`, run `docker compose restart judgebot`, not `up -d`. The file
is read only at startup. Compose recreates a container only when its configuration or
image changed, and a bind-mounted file's content is neither, so `up -d` prints `Running`
and keeps the old configuration. (Changing `JUDGE_CONFIG` itself in `.env` does change
the configuration, and `up -d` recreates.) A refresh run, scheduled or from `refresh`,
reads the file afresh, so it picks the edit up on its own. Changing `[models.embed]` is the one edit that needs the database
moved too (§7).

An older Compose may reject the `${JUDGE_CONFIG:+…}` interpolation in
`docker-compose.yml` at parse time. If so, set the containers' side by hand in the same
override file: `environment: {JUDGE_CONFIG: /etc/judgebot/judge.toml}` on `judgebot`
and `refresh`, with the mount left as it is.

### MCP endpoint

This step is optional. `judgebot` can serve the judge's tools (`crates/agent`) to an
MCP client at `/mcp`, on the same public address as the web app (§3). It needs both the `--mcp` role and an
`MCP_TOKEN`:

- `--mcp` without a token is refused at startup.
- A token without `--mcp` logs a startup warning, so a token in `.env` is never
  mistaken for a live endpoint.

There is no anonymous mode. A request without `Authorization: Bearer <MCP_TOKEN>` gets
a 401 before the protocol sees it. Behind the token are:

- `judge`: the full pipeline with model spend, under the same `JUDGE_MAX_USD` and
  `JUDGE_CONCURRENCY` as the web app.
- The agent-driven sessions: no model calls, database work only.
- The read-only lookups.

```ini
JUDGE_ROLES='--discord --api --web --mcp --jobs'   # the default roles plus --mcp; without it the token only warns
MCP_TOKEN=<openssl rand -base64 32>       # at least 24 characters, or the API refuses to start
MCP_ALLOWED_HOSTS=judge.example.com,localhost   # Host values accepted: the public host name (JUDGE_DOMAIN or the
                                                   # tunnel's), plus localhost for curl on the host; the list
                                                   # replaces the default
```

Behind the public site, a reverse proxy or the tunnel, `MCP_ALLOWED_HOSTS` must name the
public hostname. The MCP transport accepts only the `Host` values on its list, which
defaults to loopback (a guard against DNS rebinding), and each of them forwards the
hostname the client used. Without it, every `/mcp` request gets a 403
while `/api/judge` keeps working. Then run `docker compose up -d` and, from a workstation:

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

To rotate a leaked token, change it in `.env` and run `docker compose up -d`, which
recreates `judgebot` because its environment changed. Two optional extra
layers:

- Behind Cloudflare, extend §5's rate-limiting rule to `/mcp`. It costs nothing.
- Behind Cloudflare, put an Access policy (service token) in front of `/mcp`. It keeps
  unauthenticated traffic off the origin, and the bearer check stays as a second layer.

A verdict an agent persists through a session is kept only as history for that agent's
own thread. It is **never** shown to Discord or web askers as a prior-call example.
Nobody can rate it, because there is no Discord message to vote on. The answer text is
the outside agent's, and only its citations were validated.

A shell on the host can use the same tools without the network:
`docker compose run --rm --entrypoint judge-cli judgebot card "Blood Moon"`. The
`judgebot` service's entrypoint is `judgebot`, so `run` needs `--entrypoint`.

### Admin interface

This step is optional. The `--admin` role serves the operator's pages: every stored
call, private ones included, with who asked, its ratings and their raters, its run's
models and cost; the feedback queue, where every rating given a reason waits to be
resolved or dismissed (D38); the failed calls; and the usage and spend (D37). It listens on a port
of its own that the public never reaches:

- inside the container on `0.0.0.0:8789` (`ADMIN_ADDR`, set by compose);
- on the host at `127.0.0.1:8789`, published by the `proxy` service, loopback only
  (`ADMIN_PORT` in `.env` moves the host port; an `ADMIN_ADDR` there is overridden by
  compose);
- never through the public site or the tunnel, which reach `judgebot:8787` only.

It needs its own token and refuses to start without it. The token must differ from
`MCP_TOKEN` and `MCP_ADMIN_TOKEN`:

```ini
JUDGE_ROLES='--discord --api --web --admin --jobs'   # the default roles plus --admin
ADMIN_TOKEN=<openssl rand -base64 32>                 # at least 24 characters
```

Then `docker compose up -d`, and print the page's address with the token in it:

```sh
docker compose exec judgebot judge-cli admin-url
# {"url": "http://127.0.0.1:8789/admin#token=…"}
```

The token is in the address's fragment, which a browser never sends. The page keeps it
for the tab and removes it from the address bar. Open the address in a browser on the
host. From another machine, forward the port over SSH first and open the same address
there:

```sh
ssh -N -L 8789:127.0.0.1:8789 deploy-host
```

`-N` forwards the port without opening a shell; leave it running while the page is
open. With `ADMIN_PORT` set, forward that port instead of 8789. The host's sshd must
allow local forwarding: a refusal shows as `open failed: administratively prohibited`
(see Troubleshooting).

**On a trusted network** the proxy can publish the port on the host's LAN address as
well, through a `docker-compose.override.yml` beside `docker-compose.yml`. The file is
gitignored, and compose and `scripts/deploy.sh` merge it in by themselves:

```yaml
services:
  proxy:
    ports:
      - "192.168.1.50:8789:8789" # the host's LAN address : the port to open : 8789
```

`docker compose up -d proxy` applies it. The proxy is recreated, so `127.0.0.1:8787` is
down for a few seconds, or until the answers in flight finish (up to 70 s). Open the address `judge-cli admin-url` prints, with the host's
LAN address in place of `127.0.0.1`.

- The page is plain HTTP. The token crosses the network unencrypted and is the only
  guard, so anyone who can read the network's traffic can take it. The SSH forward has
  neither problem.
- Name the LAN address, not `0.0.0.0`. The override adds to the loopback mapping rather
  than replacing it, a wildcard can clash with it, and it would listen on every
  interface, a public one included.
- Docker writes its own firewall rules for published ports, ahead of ufw and firewalld,
  so a host firewall rule does not close this one. The address it binds is what limits
  who can reach it.
- The container side stays 8789 whatever `ADMIN_PORT` says; the host side is any free
  port.
- Publish 8789 only. The same override for 8787 (§3, Local only) reaches a site that
  passes `CF-Connecting-IP` through, so under `API_CLIENT_IP=cloudflare` anyone on that
  network could choose their own rate-limit bucket; and never map a LAN address to the
  container's 8788.

The proxy publishes this port whether `--admin` runs or not. When something else on
the host already has 8789, set `ADMIN_PORT` to a free port before `docker compose up`,
or the proxy cannot bind it and `127.0.0.1:8787` goes down with it.

cloudflared shares the compose network and could reach `judgebot:8789` directly. The
token is the guard on that path, which is one reason it must be long and its own.

**Agents.** The same reads are MCP tools (`admin_calls`, `admin_call`, `admin_failures`,
`admin_usage`) and `judge-cli` subcommands (`calls`, `call`, `failures`, `stats`), and
so is the feedback queue (`queue_list`, `queue_show`, `queue_resolve`, `queue_dismiss`;
`judge-cli queue`, `queue-show`, `queue-resolve`, `queue-dismiss`). The
recommended route for an agent on another machine is stdio `judge-mcp` over SSH. It
needs no token and no open port, because whoever can run it already holds the database:

```json
{ "command": "ssh", "args": ["deploy-host", "docker", "compose", "--project-directory", "/path/to/mtg-judgebot", "exec", "-T", "judgebot", "judge-mcp"] }
```

For an agent that cannot run a command (a cloud agent, a hosted connector), set
`MCP_ADMIN_TOKEN` beside `MCP_TOKEN` under `--mcp`. A request bearing it reaches the
admin tools at `/mcp` as well as the others; `MCP_TOKEN` never does. Every admin call
over `/mcp` is logged at INFO with its tool's name. Unset, `/mcp` has no admin tools.

### Card-symbol emoji

The bot draws `{W}` as a picture using *application* emoji. These belong to the
Discord application rather than to any server, and Discord stores them, so they
survive redeploys and restores. Upload them once per application, not once per deploy:

```sh
cargo run --release -p judgebot -- ingest emoji   # needs DISCORD_TOKEN; no database
```

The command is idempotent. It uploads only the symbols that are missing, so re-run it
after Scryfall adds one. Skipping it is safe: the bot logs a warning at startup
and falls back to writing `{W}` as text. The web app needs none of this, because it
loads the symbols from Scryfall's CDN.

The refresh (§7) runs the same upload. A running bot lists the emoji again:

- within ten minutes of a refresh run that uploaded any, whichever process ran it
- every ten minutes while it has none, so the first upload by hand needs no restart
- every hour regardless, so any other upload by hand, or an emoji deleted by hand, is
  seen within the hour.

### Discord over HTTP

By default the bot receives slash commands and button clicks over its gateway
connection, which one container holds at a time. A deploy hands that connection to the
new container, and Discord is quiet while it logs in. With `DISCORD_INTERACTIONS=http`
Discord POSTs each interaction to `/discord/interactions` instead, through the public
address (§3) like any other request, and they keep being answered while a deploy
switches containers (D30). The bot then
opens no gateway connection.

It needs a public HTTPS address that reaches the container (§3): `https://<JUDGE_DOMAIN>`
with the bundled site, or your proxy's or the tunnel's hostname, the one that already
serves the web app. Discord's requests come from its servers, not a browser, so they
cannot pass a challenge. Behind Cloudflare, an Access policy or a WAF challenge on that
hostname must leave `/discord/interactions` alone, and Bot Fight Mode, which covers the
whole zone and cannot skip a path, must be off. A deployment without `--api`, `--web` or
`--mcp` listens on `API_ADDR` for the endpoint and `/api/health` only, and still needs a
public route.

1. Set it in `.env` and deploy:

   ```ini
   DISCORD_INTERACTIONS=http
   ```

   ```sh
   scripts/deploy.sh
   ```

   The new container logs `receiving Discord interactions over HTTP`, and an ERROR that
   the application has no Interactions Endpoint URL. Until step 2, Discord still sends
   interactions over the gateway, which nothing holds now, so commands go unanswered.
2. In the [Developer Portal](https://discord.com/developers/applications), open the
   application's *General Information* and set *Interactions Endpoint URL* to
   `https://<your public host>/discord/interactions`. Discord tests the address when
   you save: it sends a PING and requests with invalid signatures, and refuses to save
   if any answer is wrong or missing.
3. Run `/help` in the server.

The setting and the portal must agree, because Discord delivers interactions one way
only. The bot compares them when it starts and logs an ERROR (and posts to
`JUDGE_ALERT_WEBHOOK`) if they differ, but keeps running. To go back, clear the URL
in the portal, set `DISCORD_INTERACTIONS=gateway` and deploy.

## 5. Spend protection

The model bill is most of what an instance costs, and `/api/judge` is anonymous. What
guards it depends on the route (§3).

### In the process

Every deployment has these, whatever is in front of it:

- **The per-address limit** on `/api/judge`: `API_RATE_LIMIT` questions per
  `API_RATE_WINDOW_SECS` (default 4 per 300 s), counted on the address
  `API_CLIENT_IP` names (§3, Client address). With `peer` behind a proxy it counts every
  visitor together.
- **The concurrency slots**: at most `JUDGE_CONCURRENCY` answers at once (default 2).
  A question beyond them is told the bot is busy, at no cost.
- **The spend cap**: `JUDGE_MAX_USD`, over `JUDGE_BUDGET_PERIOD` (§4). A question that
  could take the spend past it is refused before anything is sent.

The bundled HTTPS site and your own reverse proxy add nothing to these unless you
configure it (nginx's `limit_req`, say). Behind them, the cap is the backstop and the
limit the first line.

With a budget period set, a capped instance comes back by itself when the period turns.
To resume sooner, raise `JUDGE_MAX_USD` and `docker compose up -d`. The period's spend is
in the database, so the restart does not reset it.

The cap covers embeddings as well as the chat calls: each question's query embedding, a
stored call's vector, the scheduled refresh's embed step and every `judgebot ingest`
command that embeds. They bill to the same meter and the same `spend_days` rows, so
`judge-cli stats` shows them combined. At the cap a question is still answered without
vector search if its chat calls fit, and its call is stored without a vector for the next
`embed` to fill.

### At Cloudflare

Behind a Cloudflare Tunnel, the rules below are the first line of defence, at
Cloudflare's edge before a request reaches the host, and the process's own limits are
the backstop. They are Cloudflare features and apply to the tunnel's hostname only.

- **Rate limiting rule** on `/api/judge`, a burst limit in front of the process's own
  limit. The free plan has one rule, matched on the path only, counted per IP over 10
  seconds, with a 10-second block, so it cannot match the process's window:

  | Field | Value |
  | --- | --- |
  | When incoming requests match | URI Path equals `/api/judge` |
  | With the same characteristics | IP |
  | When rate exceeds | 2 requests per 10 seconds |
  | Then take action | Block, for 10 seconds |

  An answer takes 20 to 45 seconds and the web page sends one question at a time, so a
  person reaches it only by resubmitting quick replies (busy, not found, "did you
  mean?"), and then waits 10 seconds. Business and Enterprise plans can count over 5
  minutes and match the process's window; Pro counts up to 1 minute. Keep the rule to that path: Discord's requests to `/discord/interactions`
  arrive from a few addresses of Discord's, which a rule over the whole site would
  block.
- **Managed Challenge** as a WAF custom rule on the HTML document request, *not* on
  `/api/judge`. `fetch` cannot solve a challenge served to an `XHR`, so challenging the
  API path breaks the web app. Challenging the document checks a visitor once, and later
  `/api/judge` calls carry the `cf_clearance` cookie. With
  `DISCORD_INTERACTIONS=http`, keep `/discord/interactions` out of the rule too
  (match only `GET`, or exclude the path): Discord cannot pass a challenge, so its URL
  check and every interaction would fail.

Full Turnstile with server-side `siteverify` is stronger. It needs a token in the POST
body and a verification call inside `judge_route` before any spending. Add it only if
the edge rules prove insufficient.

## 6. Weekly backups to R2

The database is cheap to dump and expensive to rebuild (§2). The `backup` compose
service dumps it, uploads the dump to Cloudflare R2 and prunes old copies on its own
schedule, with no host cron and no Docker socket. `scripts/backup-db.sh` does the same
from a host cron. Both write the same objects under the same names, so `list`, `fetch`
and the restore drill work on either's backups, and the two can run side by side.

1. Create an R2 bucket.
2. Create an API token scoped to **Object Read & Write on that bucket only**.
3. Fill in the `R2_*` values in `.env.deploy`.
4. Add `backup` to `COMPOSE_PROFILES` in `.env` and bring it up:

```ini
COMPOSE_PROFILES=public,backup    # or tunnel,backup
```

```sh
docker compose up -d
docker compose logs backup          # the settings it read, then each check
```

Then **do a restore drill** (below). An untested backup is not a backup.

### Service

`judgebot backup serve` lists the bucket a few minutes after it starts, then hourly. A
backup is due when the newest `judgebot-<stamp>.dump.gz` under `BACKUP_PREFIX` is
`BACKUP_EVERY_DAYS` old (default 7), or there is none. The schedule is the bucket, so a
fresh install takes its first backup within minutes, a restart does not reset it, and a
backup the cron'd script took counts as that week's. Each backup works in this order:

1. It dumps the database (`pg_dump -Fc`, gzipped) over the compose network.
2. It refuses to upload anything under `BACKUP_MIN_BYTES`, so a stub never becomes the
   newest restore point.
3. It uploads, with the file's SHA-256 signed into the request. R2 refuses an upload
   whose bytes do not match.
4. Only then does it prune past `BACKUP_KEEP_DAYS`. It deletes only objects named like
   a backup, directly under the prefix, and never the two newest (the one just uploaded
   among them), however old: after a lapse longer than the retention, one success does
   not delete every earlier restore point.
5. It copies the dump to `BACKUP_KEEP_LOCAL`, when that is set.

Weekly backups at the default 60 days keep about eight restore points, far inside R2's
10 GB free tier.

A failed attempt is retried after an hour, then less often while it keeps failing, at
least once a day. `JUDGE_ALERT_WEBHOOK` hears the first failure of a streak, a failure
at a different step than the one before (pruning failing every week does not hide a dump
that starts failing), and the recovery, never each retry. A streak of listing failures
ends, with a note, when the bucket can be listed again and no backup is due. The container reads `.env.deploy` and not `.env`, so the
webhook goes in `.env.deploy` for the backup's alerts and in `.env` for `judgebot`'s:
set it in both to hear from both.

The settings are read once, at start. A missing or malformed one stops the container
with a line naming each, and `restart: unless-stopped` retries it, so `docker compose
ps` shows it restarting. After editing `.env.deploy`, `docker compose up -d` recreates
it.

The image carries `pg_dump` 16, the major version of the `db` service. `pg_dump` refuses
a server newer than itself, so moving `db` to a newer PostgreSQL major needs an image
built with a newer client (`PG_MAJOR` in the `Dockerfile`). Until then every attempt
fails, and the alert names both versions.

`BACKUP_KEEP_LOCAL` is a path inside the container. To keep local copies, mount a
directory the container's user (`nobody`, uid 65534) can write, and point the variable
at the mount:

```yaml
# docker-compose.override.yml
services:
  backup:
    volumes:
      - /volume1/backups/judgebot:/backups
```

```ini
BACKUP_KEEP_LOCAL=/backups
```

The script reads the same variable as a host path. If both run, give the mount the
same path on both sides, or leave the variable blank.

### Cron

`scripts/backup-db.sh` is the same backup from the host's scheduler, for a deployment
without the `backup` profile. It runs `pg_dump` inside the `db` container and uploads
with an `rclone/rclone` container. A cron entry from an earlier release keeps working
beside the service. The service counts the script's backups as its own, so beside a
weekly entry it seldom takes one itself, and an occasional second backup in a week is
pruned with the rest. Remove the entry whenever convenient.

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

The script works in the service's order: dump, size check, upload, prune past
`BACKUP_KEEP_DAYS`, local copy. Its pruning (`rclone delete --min-age`) removes every
object under the prefix older than that, not only backups, and keeps no minimum number. It posts to
`JUDGE_ALERT_WEBHOOK` when it fails.

### Restore drill

Run one backup by hand to confirm the credentials, then restore it into a scratch
database. The drill restores the object that landed in R2, not a local copy:

```sh
docker compose run --rm backup run         # take one now, whatever the schedule
docker compose run --rm --no-deps backup list        # newest last
docker compose run --rm --no-deps -T backup fetch judgebot-<stamp>.dump.gz > judgebot-<stamp>.dump.gz

docker compose exec -T db createdb -U judgebot restoretest
gunzip -c judgebot-<stamp>.dump.gz \
  | docker compose exec -T db pg_restore -U judgebot -d restoretest
docker compose exec -T db psql -U judgebot -d restoretest -c "select count(*) from cards;"
docker compose exec -T db dropdb -U judgebot restoretest
rm judgebot-<stamp>.dump.gz
```

`fetch` writes to standard output, which must not be a terminal: `-T` keeps
`docker compose run` from allocating one, which would also mangle the bytes. `--no-deps`
lets `list` and `fetch` run with the database down, as when it is the thing being
restored. Without
the `backup` profile, the script's forms are `scripts/backup-db.sh`,
`scripts/backup-db.sh list` and `scripts/backup-db.sh fetch judgebot-<stamp>.dump.gz`,
which downloads into the repository root.

The card count should match section 2. `*.dump.gz` is gitignored.

## 7. Scheduled data refresh

`judgebot` keeps the data current itself, with its `--jobs` role. Scryfall publishes new
bulk data daily, and Wizards ships a Comprehensive Rules release with most sets. Every
`JUDGE_REFRESH_HOURS` (default 24) it runs a refresh: the steps of
`judgebot ingest refresh`, in the same process, with nothing to install on the host. Its
steps, in order:

1. `cards`: Scryfall oracle cards, printed names and rulings. These are upserts, so
   cards the bot already knows are refreshed in place.
2. `rules latest`: reads Wizards' rules page, compares the linked
   `MagicCompRules <date>.txt` against `max(rules.cr_version)` and loads it only when
   the version differs.
3. `lists`: reloads the built-in alias and note lists when an upgrade changed them, and
   leaves a list you loaded from your own file alone (see below).
4. `prune`: deletes calls older than `JUDGE_CALL_RETENTION_DAYS`, with their ratings
   (see Retention below). Skipped when the variable is unset or `0`.
5. `sweep`: clears the address and user agent of web ratings older than
   `FEEDBACK_IP_DAYS` (default 90), keeping the ratings and their reasons. Every rating
   written does the same, so this step is what clears them on an instance nobody rates
   on.
6. `retire`: re-checks every stored call's citations against the data just loaded (see
   below).
7. `embed`: only rows whose text changed. The CR loader nulls the embedding of those
   rows alone, so a new CR costs the embedder a few hundred rules, not all of them.
8. `emoji`: uploads any card symbol Scryfall added. Skipped when `DISCORD_TOKEN` is
   unset.

Each step runs even if an earlier one failed. A step that cannot run for want of
configuration (`prune` with no retention, `embed` with no embedder, `emoji` with no
`DISCORD_TOKEN`) logs
`refresh step skipped` with the reason and does not count as a failure. Most days a run
costs one Scryfall download and nothing else. It takes a few minutes on a NAS, mostly
parsing the `default_cards` file. The CR load is one transaction, so retrieval sees the
old rules or the new, never a mix. The `cards` step commits in batches: each card
changes together with its faces, and the rulings change in one transaction after the
cards. A `cards` step cut short leaves some cards refreshed, and the next run finishes
the rest.

### Retention

`JUDGE_CALL_RETENTION_DAYS` bounds how long answered calls are kept: 1 to 3650 days,
unset or `0` to keep every call (the default). 90 is a sensible choice. The refresh's
`prune` step deletes calls past it with their ratings, as does `judgebot ingest prune`
by hand. With `JUDGE_REFRESH_HOURS=0`, cron one of the two.

- **Usage survives.** Each run's usage record (`judge_runs`: outcome, models, tokens,
  cost) stays, unlinked from the deleted call, and so does the daily spend ledger.
  `judge-cli stats` counts questions and ratings from the stored calls, so for days past
  the retention it shows the spend and fewer (or no) questions.
- **Examples go.** A deleted call is no longer a prior-call example or part of a
  thread's history, so a well-rated old answer stops guiding new ones.
- **Citations stay.** A newer call that cited a deleted one keeps that citation, and
  the retirement pass does not retire it for it.
- **Ratings stop.** A rating button on a deleted answer replies that it is no longer
  stored.
- **Failed calls** have their own bound: 30 days or the newest 500.

A value out of range stops every binary that loads the configuration.

### Schedule

The schedule lives in the database, so every process with `--jobs`, a restart and a cron
job all agree on it:

- A run is due when the last *successful* run finished more than `JUDGE_REFRESH_HOURS`
  ago (or none ever has), and the last attempt started long enough ago: an hour after
  one failure, doubling with each further failure in a row (2, 4, 8 hours…), never more
  than `JUDGE_REFRESH_HOURS`. Every age is measured on Postgres's clock.
- A run whose process died (killed, out of memory) leaves its row unfinished. After
  3 hours 10 minutes the schedule counts it as a failed run: it backs off as for any
  failure, and the webhook is told once that a refresh never finished, not again on
  each attempt of a crash loop.
- Each process first checks 1.5 to 3.5 minutes after it starts, then every 10 to 12
  minutes.
  An instance whose data was loaded but never refreshed catches up within minutes.
- Only one run happens at a time. A due check takes the refresh lease (below) without
  waiting. If another process or a cron run holds it, the check does nothing. The
  process that runs it records itself in `refresh_runs.process`.
- The run has its own thread, async runtime and pool of three database connections,
  so its downloads, file parsing and queries never take a worker or a connection a
  question needs. The database itself is shared. While the CR load or the retirement
  pass holds the calls lock for its transaction, saving an answer with its vector
  waits for it, and the reply waits with it. A cron run does the same.
- A run stops after three hours. The step in progress is abandoned and recorded as
  failed with `timed out`. Its transaction rolls back unless it was committing. The
  steps after it are skipped, and the lease is released, so a hung download cannot
  stop every later refresh. A statement the abandoned step had already sent runs on in
  Postgres until it ends, so the schedule's connections set `statement_timeout` (30
  minutes) and `lock_timeout` (15 minutes): no statement of a refresh comes near either
  on a NAS, and either ends a stuck one, with any lock it held.
- If a run is still going 3 hours 10 minutes after it started, the scheduler drops it,
  marks its row failed and releases the lease. Only a run stuck outside its steps gets
  that far.
- A container asked to stop (`docker stop`, every `scripts/deploy.sh`) stops its
  refresh too. The step in progress is cut off (its transaction rolls back), the rest
  are skipped, the run is recorded as stopped, neither a success nor a failure (no
  alert, no backoff), and the lease is released. The new container's first check, 1.5 to
  3.5 minutes after it starts, finds the refresh still due and runs it from the start.
  A statement the cut step had sent is ended by Postgres within seconds of the old
  container's connections closing (`client_connection_check_interval`, Postgres 14 on),
  so the answers still draining do not wait on its lock. The log says `shutting down:
  the refresh step in progress is cut off` and `the scheduled refresh stopped`. A
  container that is killed rather than stopped leaves its run open, as a crash does.
- A process whose binary does not match the schema never writes. A container left
  running on an older image after a newer release migrated the database logs
  `scheduled refresh paused: a newer release migrated the database`, and one whose
  migrations are pending (`JUDGE_AUTO_MIGRATE=false`) logs `scheduled refresh paused:
  the schema is behind this binary`. Each resumes by itself once the two match. A run
  that meets a schema change part-way (a deploy during a refresh) stops before its next
  step and is recorded as stopped, neither a success nor a failure: no alert, no
  backoff. A manual or cron `judgebot ingest refresh` checks the same before every step,
  and a single-step command (`cards`, `rules`, `embed`, …) once before it starts. Both
  exit non-zero instead of writing.
- A database with no rules loaded has not had its first load. The schedule waits for
  `docker compose run --rm refresh init` (`scheduled refresh paused: no rules are
  loaded`) rather than running a refresh, which would skip embedding every row at the
  spend guard.

At startup each process logs `scheduled data refresh on` with the schedule, the age of
the last success and the stored CR version. A run logs `refresh starting` and
`refresh finished` with `trigger=schedule`, and one line per step in between.

`JUDGE_REFRESH_HOURS` takes 1 to 720 hours, and `0` turns the schedule off. A typo or a
negative value stops every binary that loads the configuration (`judgebot`,
`judge-cli`, `judge-mcp`, `judge-eval`) at startup, naming the
variable, as `JUDGE_BUDGET_PERIOD` does. After changing it, `docker compose up -d`
recreates `judgebot`.

With `JUDGE_ALERT_WEBHOOK` set, a scheduled run posts there:

- the first failure of a streak, naming the failed steps and saying so when it timed
  out (the log has the errors). A retry that fails again is not reposted.
- the first success after a failure, whoever's run failed.
- an `embed` step skipped by the spend guard (below), with the command that does it by
  hand. A run after one that skipped too is not reposted.
- a check that crashed (a panic), once until a run succeeds.
- a run that never finished (its process died), once per streak of failures.

**The spend guard.** A scheduled run never pays for a mass re-embed. A manual or cron
`judgebot ingest refresh` (`scripts/refresh-data.sh`) has no such ceiling: whoever installed
it chose to run it. Before embedding
it counts the rows waiting for a vector. Above 800 it skips the step
(`refresh step skipped`, with the count) and alerts. A full re-embed is about 1,900 rows
(1,173 rules, 739 glossary entries and one per stored call), while a new CR release empties
a few hundred. More than 800 empty vectors means something else emptied them: an
embedder configured on a database that never had one, or a `reembed --clear` that
died. Run `scripts/refresh-data.sh embed` when that spend is expected. The ceiling bounds
a run whatever the spend cap says. Below it, the embed step is billed to the cap like
everything else, and a cap that cannot fit a batch fails the step, which alerts as any
failed step does.

### Cron

`scripts/refresh-data.sh` runs the same refresh from the host, as
`docker compose run --rm refresh`. It checks that the image `docker compose pull`
fetched is present and never builds on the host. Any argument is passed through as the
`judgebot ingest` subcommand: `scripts/refresh-data.sh rules latest` checks only the CR. It
exits non-zero if any step failed, and posts to `JUDGE_ALERT_WEBHOOK` when it does.

A cron entry from an earlier release keeps working. It takes the same lease and writes
the same record, so the schedule counts its run as that day's and never overlaps it.
Remove the entry whenever convenient. To keep cron in charge instead, set
`JUDGE_REFRESH_HOURS=0` and install the entry on the host's scheduler (§6, Cron, has the
Synology notes: run as root, absolute paths, tick email-on-error):

```sh
crontab -e
30 5 * * *  /path/to/mtg-judgebot/scripts/refresh-data.sh >> ~/judgebot-refresh.log 2>&1
```

### Lease

Runs never overlap. Every `judgebot ingest` command that writes data takes the refresh
lease first, an advisory lock in the database. If another run holds it, the command logs
`another refresh or ingest step holds the refresh lease; waiting for it to finish`, naming
the holder, and waits. So the scheduled run, `scripts/refresh-data.sh <step>` and a
manual `judgebot ingest` from a workstation take turns. `migrate` and `emoji` do not take
it: `migrate` has its own lock, and `emoji` writes no database.

- Postgres drops the lock when the holding session ends, so a run that crashed or was
  killed leaves nothing to clean up.
- A command's wait is bounded at an hour. After that it fails with
  `the refresh lease is still held after 60 min, by pid …`, so a hung run makes the
  next one fail and alert instead of queueing every later run behind it. The schedule
  never waits: it checks again ten minutes later.
- Downloads give up after 30 seconds without a connection or two minutes without data,
  so a stalled Scryfall or Wizards connection fails its step instead of hanging the run.
- A run checks before each step that it still holds the lease. If the lease's session
  was ended (a database restart, `pg_terminate_backend`), the run stops and records the
  remaining steps as failed with `refresh lease lost`.

### Run record

Each run is recorded in `refresh_runs`: when it started and finished, what started it
(`schedule` or `manual`; `init` records itself as a manual run of the steps it shares
with a refresh) and in which process, the CR version before and after, and each
step's outcome. A row with no `finished_at` is a run in progress or one that died. A
row with `ok` null is a run that stopped: the schema was not its binary's, or its
container was stopping (its step in progress recorded as `interrupted`, the rest as
`shutdown`). A run that loads a new CR also logs `CR <old> → <new>`.

`judge-cli stats` lists the last five runs (`refresh_runs` in its JSON): outcome, CR
before and after, and the steps that failed. On a schema without the table (migrations
pending) the list is empty and `refresh_runs_note` says why. Users see the result: `/help`, the web
footer and `GET /api/about` show the CR release loaded and how long ago the last
successful refresh finished, and say when the latest one failed. The full record is in
SQL:

```sql
select started_at, finished_at, trigger, process, ok, cr_before, cr_after from refresh_runs order by id desc limit 5;
select s from refresh_runs, jsonb_array_elements(steps) s where id = (select max(id) from refresh_runs);
```

A run reads the same `judge.toml` the serving roles do (compose mounts it from
`JUDGE_CONFIG`, §4), afresh each time. Its `embed` step writes vectors in the space the
bot queries, and refuses when the configured space and the database's disagree.
Downloads are cached in the `judgebot-ingest-cache` volume, which `judgebot` and
`refresh` share.

### Call retirement

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

Between the `rules` and `embed` steps (about a minute), the vector search cannot find
the changed rules. If `embed` fails (the embedder is down or rate-limited), those rules
stay unembedded until the next successful run, because `embed` always fills every NULL.

To watch a run, read `docker compose logs judgebot | grep refresh` after the first
check, or start one now with `scripts/refresh-data.sh`. Expect one line per step,
`cards`, `rules`, `lists`, `prune`, `sweep`, `retire`, `embed` and `emoji` in that order, each
`refresh step ok`
or `refresh step skipped` and none `refresh step failed`. A run that loaded a new CR
then logs `CR <old> → <new>` last. A one-off manual load also works from a workstation
(`cargo run --release -p judgebot -- ingest rules <url>`). To force a re-parse of an
already-loaded version that way, delete the cached txt first.

### Curated lists

The nickname list (`card_aliases`) and the notes on hard cards (`card_notes`) come from
`data/aliases.yaml` and `data/notes.yaml`, which are compiled into the binary. Each load
records where its list came from in `curated_lists`: `builtin` for `init` and for
`judgebot ingest aliases` / `notes` with no file, `file` when you name one. It also
records the SHA-256 of the YAML loaded. The `lists` step reads that record for each list:

- `builtin`, and the binary's copy is the same: the table is compared with what that
  copy loads against today's cards. Equal is `current`. Otherwise it is reloaded
  (`reloaded`): a list loaded before the cards were, or naming a card Scryfall added or
  restored since, fills in by itself.
- `builtin`, and the binary's copy differs (an upgrade changed it): reloaded from the
  binary's copy (`reloaded`).
- `file`: your own list, never touched (`kept`, reason `file`).

**Loading a file opts that list out of built-in updates**, even a file identical to the
built-in copy, until you load it again with no file. Each run logs this at INFO.

A list loaded before the record existed has none. The step reloads it, recording it as
built-in (`adopted`), only when its table holds exactly what the binary's copy, or an
earlier release's built-in copy, loads. A list with anything added, removed or pointed
at another card is kept (reason `unrecorded`), and each run logs a warning until you
settle it:

- `judgebot ingest aliases <file>` (or `notes <file>`) records your list as yours.
- `judgebot ingest aliases` (or `notes`) with no file loads the built-in copy, which the
  refresh then keeps current.

The image has no `data/` directory, so on a deploy host mount the file, from the
repository root: `docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml`. `init`
keeps a list you loaded from a file, with a warning, and loads the built-in copy of the
other.

### Embedding model change

Vectors from two models cannot share a column. The database therefore records which
model's vectors it holds (`embedding_space`, one row: provider kind, model, width), and
every reader and writer checks it first. If a bot's `[models.embed]` names a different
model or width, it does not mix them. It logs `embedding space mismatch; vector search off` and
answers from the curated map and full-text search alone until the two agree.

`judgebot ingest reembed` moves the database to a new model. It pays the provider to
embed every rule, glossary entry and stored call again. So it is a dry run by default,
and you should take a backup first.

```sh
docker compose run --rm backup run            # a restore point holding the old vectors (§6;
                                              # or scripts/backup-db.sh)
$EDITOR judge.toml                            # [models.embed]: the new provider/model/dimensions
scripts/refresh-data.sh reembed               # dry run: what is stored, what would be cleared,
                                              # rows, the cost and what the spend cap has
                                              # left; probes the new model once; exits
                                              # non-zero having changed nothing
scripts/refresh-data.sh reembed --yes         # one transaction: retype vector(N), rebuild the
                                              # HNSW indexes, clear every vector, rewrite the
                                              # row — then the ordinary embed loop
docker compose restart judgebot               # judgebot reads judge.toml once, at startup;
                                              # `up -d` would see nothing to do (§4)
```

The running `judgebot` keeps the `[models.embed]` it started with, so it must be
restarted at some point. It re-reads the space row on every request. Its vector search
turns off as soon as the row disagrees with its configuration and back on as soon as
it agrees. Either order works:

- Restarting before `--yes` turns it off from the restart until the switch.
- Restarting after `--yes` turns it off from the switch until the restart.

Either way there is one window without vectors, and no mixing. The order above keeps it short.

The refill is resumable. If the embed loop dies (rate limit, a provider outage),
`scripts/refresh-data.sh embed` or the next scheduled run fills whatever is still NULL
(up to the spend guard's 800 rows, §7).
Retrieval degrades to the other sources for the rows not yet embedded. `reembed --yes`
resumes too: with the row already switched it has nothing to switch, so it fills the
empty rows and pays for nothing twice.

`--clear` clears and re-buys every vector in the same space. The row does not change,
so the running bot logs no mismatch, and its vector search finds nothing until the refill
finishes.

A resumed refill and a scheduled refresh never overlap: both take the refresh lease,
so whichever starts second waits for the first.

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
`judgebot ingest` inside the `refresh` container, which already has both.

**The spend cap applies to the re-embed.** The dry run prints the estimated cost at the
configured price (four bytes a token), its worst case (what the cap reserves: a token a
byte) and what is left under `JUDGE_MAX_USD`. With `JUDGE_BUDGET_PERIOD=day|month`, what
is left is the period's, the bot's questions included. When the worst case does not fit,
it names a cap that does, and `--yes` refuses to switch: nothing is cleared until the
whole refill fits. Raise the cap for that one run, leaving `.env` alone:

```sh
docker compose run --rm -e JUDGE_MAX_USD=20 refresh reembed --yes
```

Under `day` or `month` that run spends the period's budget. Its spend reaches the ledger,
so once the bot's own cap (still the `.env` value) sees the period total, the bot may
refuse questions for the rest of the day or month. Run a large re-embed early in a
period, or raise `JUDGE_MAX_USD` in `.env` too and `docker compose up -d` until it turns.

A refill of rows already empty is not refused up front: it stops with `stopped at the
spend cap`, keeping every row it embedded, and the same command with room resumes it.
The run's spend is written to the ledger, so `judge-cli stats` shows it.

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
  docker compose up -d --build judgebot
```

A `JUDGE_COMMIT` that is not a commit id (a branch name, a tag) fails the build rather
than becoming "commit unknown" on every interface.

A GitHub release tagged `vX.Y.Z` adds version tags to the image already built for that
commit, without rebuilding it: `X.Y.Z`, `X.Y` and, from 1.0 on, `X`. The release is
exactly the image that has been running as `latest`, down to the platform digests. To
update on releases rather than on every push, pin one:

```ini
JUDGE_IMAGE_TAG=1.2    # in .env: follows 1.2.x patch releases; 1.2.0 pins one exactly
```

`CONTRIBUTING.md` says how a release is cut. The ordinary deploy has no downtime
(D29):

```sh
git pull                                # runbook + compose changes
scripts/deploy.sh                       # pull, start the new container, stop the old
```

`scripts/deploy.sh` pulls the image and starts a second `judgebot` container from it
beside the running one:

1. The new container applies the release's migrations, if any, and starts its roles.
   Its HTTP roles serve at once. Its Discord role stands by, because the old container
   holds the gateway. With `DISCORD_INTERACTIONS=http` there is no gateway: the
   Discord role reads the application's public key and answers interactions as they
   arrive, beside the old container.
2. When Docker reports it healthy (its first passing healthcheck), the script stops the old container. `docker stop` sends SIGTERM and the old
   container drains. It closes its listener and its Discord gateway connection, releases
   the gateway lease, and finishes the answers in flight, for up to two minutes. Then
   it exits.
3. The new container connects to Discord as soon as the lease is released. The old
   one records that it closed its gateway, so the new one skips the 15-second grace
   that a takeover after a crash waits. Questions asked on Discord during the new
   container's login, a few seconds, get Discord's "The application did not respond".
   An old container from a release before this record (the first deploy onto it)
   leaves none, and the new one waits out the grace. The web app and the API see no
   gap: the `proxy` service, `edge` and the tunnel reach whichever container is
   listening.
   Neither does Discord with `DISCORD_INTERACTIONS=http` ([Discord over HTTP](#discord-over-http)).

If the new container is not healthy within five minutes (`DEPLOY_WAIT_SECS`), or
restarts, the script removes it, prints its last log lines and exits non-zero. The old
container keeps serving. `scripts/deploy.sh --build` builds the image from the checkout
instead of pulling it.

"Healthy" means `GET /api/health` answers, which needs a listener: an HTTP role, or
`DISCORD_INTERACTIONS=http`, where it also waits for the interactions endpoint to have
the application's public key. A container with neither (`JUDGE_ROLES=--discord --jobs`
over the gateway) passes its healthcheck at once, so the script switches to it before
anything shows that it can connect to Discord. Check its log for the gateway connection
after the deploy.

The deploy that stops the old container first,
`docker compose pull && docker compose up -d --remove-orphans`, still works for every
release, with a gap while the new container starts. `scripts/deploy.sh` covers the two
cases that once needed it: the upgrade to one `judgebot` service below (it finds no
`judgebot` container and runs that same `up -d --remove-orphans`, after a backup when
one is configured), and a major release (Releases with a migration, below).

**`--remove-orphans` is required on the upgrade that replaced the `bot` and `api`
services with `judgebot`.** Without it the old `judgebot-bot` and `judgebot-api`
containers keep running, because they belong to services the compose file no longer
names. What happens next depends on what was running:

- With `judgebot-api` running, the new container cannot start, because `judgebot-api`
  still holds port 8787 and `up -d` fails.
- With only `judgebot-bot` running (a Discord-only deployment that started `bot` alone,
  or an `api` that was stopped or crash-looping), `up -d` succeeds and exits 0.
  `judgebot-bot` keeps answering Discord beside the new process on the same token, and
  every question is answered twice. Nothing in the output says so. The new process
  takes the gateway lease (below), but the old `bot` image does not know it exists, so
  the lease cannot keep it off the gateway.

Stopping `judgebot-api` by hand to free the port leads to the second case.
`--remove-orphans` removes both before the new container starts. The flag is harmless on
every other deploy, so keep it.

Check afterwards:

```sh
docker ps -a --filter name=judgebot- --format '{{.Names}} {{.Status}}'
```

It should list `judgebot-db`, `judgebot-proxy` (and `judgebot-edge` or `judgebot-tunnel`
with their profile)
and the service's own container, which Compose names after the checkout's directory
(`mtg-judgebot-judgebot-1`). A `judgebot-bot` or `judgebot-api` is left over. If either
is still there, `docker rm -f judgebot-bot judgebot-api` removes it.

What else that upgrade changes:

- **Roles.** A `.env` with neither `JUDGE_ROLES` nor `API_INTERFACES` runs
  `--discord --api --web --jobs`, what the two services ran together. `judgebot` refuses
  to start while `API_INTERFACES` is set: replace it with `JUDGE_ROLES` set to
  `--discord --jobs` plus its value (the error names the line).
- **A deployment without Discord** (one that ran only `api`, with no `DISCORD_TOKEN`)
  adds this line to `.env` before `up -d`, or `judgebot` restarts forever with
  `DISCORD_TOKEN is not set`:

  ```ini
  JUDGE_ROLES='--api --web --jobs'
  ```
- **The image tag.** The new compose file runs the `judgebot` binary, which older images
  do not have. A `JUDGE_IMAGE_TAG` pinned to an earlier release moves to this one in the
  same deploy, or `up -d` fails with `judgebot` not found. For the same reason, run
  `docker compose pull` straight after `git pull`: a cron'd `scripts/refresh-data.sh`
  between the two runs the new compose file on the old local image and fails.
- **The tunnel.** `api` on the compose network is the `proxy` service, which forwards to
  `judgebot`, so a public hostname pointing at `http://api:8787` needs no edit.
- **The host port.** `127.0.0.1:8787` is the `proxy` service. The `judgebot` container
  has no fixed name, so use the service name with Compose (`docker compose logs
  judgebot`, `docker compose exec judgebot …`) rather than `docker logs judgebot`.
- **Commands.** `scripts/refresh-data.sh`, its cron entry and
  `docker compose run --rm refresh <command>` work as before. The logs are
  `docker compose logs judgebot`, and a `judge.toml` edit takes
  `docker compose restart judgebot`.

`docker compose up -d --build` still works on a machine with the CPU and RAM for it.
The compose file keeps `build: .` for local development.

### Gateway holder

Only one process connects to the Discord gateway: the one holding the gateway lease, a
Postgres advisory lock. Any other `judgebot --discord` on the same database (a second
container, one on another host, a new container started before the old one stopped)
logs this and waits, while its HTTP roles serve:

```text
standing by: another instance holds the Discord gateway; this one connects when it lets go holder=pid 4242 ("judgebot gateway (judgebot) since 2026-10-09 14:02Z", …)
```

- **Takeover.** When the holder exits or crashes, its database connection closes and a
  standby takes the lease, waits a grace period of 15 seconds and connects. A holder
  that shut down cleanly skips the wait for its standby (Shutting down, below). A holder
  that vanishes without closing its connection (power loss, a network partition, a
  container network torn down) is noticed by the database's TCP keepalives, which every
  lease session turns on: its session ends within about 25 seconds, so the standby
  connects about 25 seconds plus the grace later. Questions asked in that gap, the
  grace plus Discord's login, get Discord's "The application did not respond". A
  "did you mean?" pick offered by the old holder still works: the new holder answers it
  from the message, within ten minutes of the prompt.
- **A lost lease.** The holder checks every 5 seconds that its session still holds the
  lock. When the check fails (the database restarted, the session was ended with
  `pg_terminate_backend`) or gets no answer in 4 seconds, it closes its gateway
  connection and stands by again, while the process's HTTP roles keep serving. A
  restart of the database therefore costs the bot a grace period and a login, and
  costs the web app nothing. If the gateway does not close within 1 second, the process
  exits non-zero instead, which closes it, and `restart: unless-stopped` brings it
  back. The grace (15 seconds) is longer than the slowest disconnect (a 5-second
  interval, a 4-second check and a 1-second shutdown), so the old and new holders
  never answer side by side.
- **Shutting down.** A holder sent SIGTERM (a deploy's `docker stop`, Ctrl-C) closes its
  gateway connection and releases the lease at once, then finishes its answers in
  flight over Discord's REST API, which needs no gateway. A gateway that closed within
  a second is recorded as a clean release (table `gateway_handover`), and a standby
  that takes the lease within 5 seconds of it connects at once, without the grace. A
  holder whose gateway is slow to close keeps the lease until it has closed, or for at
  most the two minutes of the drain, so the two never answer side by side, and its
  standby waits out the grace. A standby sent SIGTERM stops waiting.
- **Repeated losses.** Every connection spends one of the 1000 logins a day Discord
  allows a token before it resets it. A lease lost again within 30 minutes of the
  last connection (a database restarting in a loop) makes the holder pause before
  standing by again, 30 seconds and doubling to 10 minutes, with an ERROR line
  `the Discord gateway lease was lost again soon after the last loss`. Half an hour
  connected resets it.
- **A standby's own connection.** A standby whose database connection drops reconnects
  with a backoff (1 second, doubling to 30) and keeps standing by. It never exits for
  that alone.
- **A frozen holder.** A holder whose process is suspended (`docker pause`, SIGSTOP, a
  paused VM) cannot run its check. A paused container or a stopped process still has a
  running kernel, which keeps its database session and its lock alive: the bot is
  offline until it resumes, with no overlap. A paused VM stops answering the
  keepalives and loses its session in about 25 seconds, and a standby takes over. The
  resumed holder then answers beside the new one until its next check disconnects it,
  within 10 seconds of resuming. Unpausing a VM that held the gateway is the one case to
  watch.
- **A frozen standby.** A standby paused while it waits can be granted the lock during
  its current request (up to 60 seconds each). The bot is then offline until the
  standby resumes and waits out its grace, with no overlap.
- **A holder cut off from the database** fails its next check and disconnects
  within the check timeout plus the shutdown (5 seconds) of it, and stands by. Postgres frees its lock
  when the keepalives give up on the session, so the standby takes over about 25
  seconds plus the grace later, never earlier.
- **One database.** The lease lives in the database, so it protects only processes
  that share one. Two instances with separate databases and one token both answer.
- **Older images.** An image from before the single `judgebot` service never takes the
  lease, so the upgrade to it still needs `--remove-orphans` (above), and so does a
  rollback that far. Any number of `--discord` processes on this image or later are safe.
- **A direct connection.** The gateway and refresh leases are session-level advisory
  locks, held by one database connection for as long as it lives. `DATABASE_URL` must
  reach Postgres directly, or through a pooler in session mode. A pooler in transaction
  mode (PgBouncer's `pool_mode = transaction`) hands that connection to other clients
  between statements, and the lock with it. Each lease session turns off
  `statement_timeout`, `idle_session_timeout` and (Postgres 17) `transaction_timeout`
  for itself, so a server- or role-wide setting of those cannot end it either; the
  other connections keep them.

The holder and any standbys, by `application_name`:

```sql
select pid, application_name, backend_start, query_start, state
from pg_stat_activity where application_name like 'judgebot gateway%';
```

The holder reads `judgebot gateway (<process>) since <UTC minute>`, a standby
`judgebot gateway (<process>) waiting`, waiting since `query_start`.

### Releases with a migration

The ordinary deploy above is complete for a release that adds a file under
`crates/bot/migrations/`. `judgebot` applies pending migrations at startup, before
anything else touches the database. The calls-rewrite advisory lock keeps it from
running beside a refresh's CR load, and sqlx's own migrator lock is a second layer
against a concurrent `sqlx migrate run`. The log says `schema migrated` with the versions.

A failed migration exits the process. Under `restart: unless-stopped` that is a
crash-loop, with the reason in `docker compose logs judgebot`. This is loud on purpose:
a bot running against the wrong schema would answer questions and quietly fail to save
them.

The refresh job's CR load, retirement pass and embedding writes take the same advisory
lock, so the migration and those steps wait for each other. (The Scryfall card/rulings
upsert takes no lock and needs none: it commits in batches of whole cards and touches no
`calls` rows.) A deploy during a refresh therefore waits at startup until the CR load
finishes, which is minutes on a NAS. It logs one warning line,
`another job holds the calls rewrite lock ... waiting`, in `docker compose logs judgebot`.
That is a wait, not a hang. A stop-then-start deploy (`docker compose up -d`) does not
wait: the old container cuts the load off as it stops.

`scripts/deploy.sh` starts the new container while the old one still answers, so the
old binary runs against the new schema for a minute or two. Within a major release,
migrations are written for that (`CONTRIBUTING.md`, D29): they add tables, columns that
may be null or have a default, and indexes, and leave a rename or a drop to the next
major.

A major release's migrations may change what the previous release reads, so they wait
for it to stop (D32). The deploy is the same command, `scripts/deploy.sh`, and it says
which path it takes (`the new image's migrations: major`):

1. With `R2_BUCKET` set in `.env.deploy` it takes a backup first, the way back if the
   upgrade has to be undone (`DEPLOY_SKIP_BACKUP=1` skips it).
2. It starts the new container beside the old one. The new one answers every question
   with a maintenance reply and a way to retry (Discord's Retry button, the web app's
   Try again, a `maintenance` reply with `Retry-After` from the API) and waits.
3. As soon as the new one listens, it stops the old one, which finishes the answers in
   flight.
4. Once every `judgebot` of the previous release has exited (each process with a role
   holds a lock while it runs; a one-shot `judgebot ingest` stops at its next step), the
   new one migrates,
   starts answering, and becomes healthy. The log says `waiting for the previous
   release's processes to stop before migrating` with their names until then.

Questions get the maintenance reply for the drain and the migration, usually a minute
or two. From a 1.x release, whose processes hold no lock the new one could wait for,
the old container is stopped before the new one starts. `docker compose pull &&
docker compose up -d` also works: it stops the old container first, and the new one
answers with the maintenance reply while it migrates.

Nothing stops another process on the same database. A `judgebot` of the previous
release on another machine holds the lock too, so the new one waits for it: stop it.
A `judge-cli` or `judge-mcp` of the previous major stops by itself and says what to
update. `ALTER TABLE` also waits on any in-flight query.

To migrate by hand instead, set `JUDGE_AUTO_MIGRATE=false` in `.env`. `judgebot`
picks it up when next recreated, which `up -d` does because the env file changed. Then run the explicit form from the *new* image:

```sh
docker compose pull
docker compose stop judgebot              # only when the migration rewrites rows
docker compose run --rm refresh migrate   # prints what it applies; refuses a changed file
docker compose up -d
```

Until the migrations are applied, a `judgebot` whose code reads a new table or function
fails the requests that read it with a `500` (2.0.0's `publicly_visible`: the feed, web
rating, the dashboard and the admin dashboard), while the rest of it answers.

A major release's migrations wait for the previous release to stop, so with
`JUDGE_AUTO_MIGRATE=false` stop it before `migrate` (from 1.x:
`docker stop judgebot-bot judgebot-api`). `scripts/deploy.sh` refuses a major with the
setting off before it changes anything, since the new container would never migrate,
and names these steps. Run it again once `migrate` has run: it then deploys in place.

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

Every build leaves an immutable tag, so reverting a bad deploy is a one-line change,
within a major release (below). Take the
`sha-<short>` from the workflow run summary, or the version of the last good release:

```ini
JUDGE_IMAGE_TAG=sha-abc1234    # in .env; or a release, e.g. 1.2.0
```

```sh
docker compose pull && docker compose up -d --remove-orphans
```

Clear `JUDGE_IMAGE_TAG` to return to `latest`.

An image from before `judgebot` (the release that ran `bot` and `api` as two services)
has no `judgebot` binary, so the current `docker-compose.yml` cannot start it. Rolling
back that far takes that release's compose file as well as its image:

```sh
git checkout <previous release tag or commit> -- docker-compose.yml
# JUDGE_IMAGE_TAG=<that release> in .env
docker compose pull && docker compose up -d --remove-orphans
```

`--remove-orphans` matters here too: the `judgebot` container is an orphan of the old
file, and left running it answers Discord beside the restored `bot`.
`git checkout HEAD -- docker-compose.yml` undoes it when you upgrade again.

An older tag starting against a newer schema depends on how far back it goes:

- **Within its major release** (a `latest` build of the next minor included), it logs
  `database is ahead of this binary`, does not migrate, and runs: migrations within a
  major only add (`CONTRIBUTING.md`), so its queries still find what they need.
- **Across a major release**, or from one build of a new major's pre-release to another
  (`latest` while `main` is on `3.0.0-dev`, say), it refuses to start and exits, naming
  the build that migrated the database and its own. From 2.0 on: a 1.x image has no
  such check, starts, and fails on the queries the new schema broke. Rolling back that
  far means restoring the backup taken before the upgrade too. Take one by hand right
  before a major release's deploy (`docker compose run --rm backup run`, or
  `scripts/backup-db.sh`).

A major's backup is restored into an empty database: `pg_restore --clean` cannot drop
the old release's tables while the new release's tables reference them. Stop `judgebot`
first, or it migrates the restored database again, then restore, then start the older
release's compose file and image:

```sh
docker compose stop judgebot
docker compose run --rm --no-deps -T backup fetch judgebot-<stamp>.dump.gz \
  > judgebot-<stamp>.dump.gz              # or scripts/backup-db.sh fetch <name>
docker compose exec -T db dropdb -U judgebot --force judgebot
docker compose exec -T db createdb -U judgebot judgebot
gunzip -c judgebot-<stamp>.dump.gz \
  | docker compose exec -T db pg_restore -U judgebot -d judgebot
# then the older release's docker-compose.yml and JUDGE_IMAGE_TAG, as above
docker compose pull && docker compose up -d --remove-orphans
```

`judgebot ingest migrate` refuses a database that is ahead rather than guessing, and the
scheduled refresh stays paused while it is (§7): the data is not refreshed until the
image and the schema match again. An image from before the built-in schedule refreshes
nothing by itself, so a rollback that far needs the cron entry of §7 until you upgrade.

`edge`, `cloudflared` and `db` are untouched by a code deploy. The tunnel reconnects
on its own if the connector restarts.

## 9. Troubleshooting

| Symptom | Cause |
| --- | --- |
| 502 from the public hostname | `judgebot` is down, restarting or runs no network role (`docker compose ps`; under `public`, `edge` answers 502 for it), or the tunnel's service is not `http://proxy:8787` (or `http://api:8787`) |
| `edge` restarts, logging `set JUDGE_DOMAIN in .env` | the `public` profile is on and `JUDGE_DOMAIN` is empty or not a host name (two or more labels, not `localhost` or an IP address, no `https://`, port, path or trailing dot) |
| `edge` will not start: `port is already allocated` for 80 or 443 | something else on the host serves the web ports; stop it, or put your own reverse proxy in front of `127.0.0.1:8787` instead (§3) |
| The public site answers with a certificate error, and `docker compose logs edge` shows `challenge failed` or a timeout | Let's Encrypt cannot reach the host under `JUDGE_DOMAIN`: ports 80 and 443 are not forwarded to it (or a firewall or the ISP blocks them), or the `A`/`AAAA` record does not point at the host (`dig +short judge.example.com`; an `AAAA` to another machine fails too) |
| `docker compose logs edge` shows the certificate order `rejected`, `rateLimited` or `caa` | Let's Encrypt refused the name: too many failed attempts or certificates for it lately (wait for the limit's window; stop restarting `edge`), or a `CAA` record on the domain names another authority |
| Tunnel healthy, hostname NXDOMAIN | the `judge` record is grey-clouded; it must be proxied |
| `/mcp` answers 401 | wrong or missing `Authorization: Bearer <MCP_TOKEN>` |
| `/mcp` answers 403 while `/api/health` is fine | the public hostname is not in `MCP_ALLOWED_HOSTS` |
| `/mcp` answers 405 (a browser GET shows the web app) | `--mcp` is not in `JUDGE_ROLES`, so `/mcp` is just another page path |
| `judgebot` exits naming `--mcp` and `MCP_TOKEN` | `--mcp` with no token to gate it; set `MCP_TOKEN` or drop the flag |
| `/mcp` 404s and the log warns about `MCP_TOKEN` | the token is set but `--mcp` is not in `JUDGE_ROLES` |
| `judgebot` exits naming `--admin` and `ADMIN_TOKEN` | `--admin` with no token, a short one, or one equal to `MCP_TOKEN` or `MCP_ADMIN_TOKEN` |
| `127.0.0.1:8789` answers 502 | `--admin` is not in `JUDGE_ROLES`, or `judgebot` is restarting |
| The `proxy` will not start: `port is already allocated` for 8789 | something else on the host has 8789; set `ADMIN_PORT` in `.env` to a free port |
| `judgebot` exits saying `ADMIN_ADDR` and `API_ADDR` name the same port | the two listeners need ports of their own |
| The admin page does not load from another machine through the override | the mapping names `0.0.0.0` or the wrong address (`docker compose ps proxy` lists what is published), or `docker compose up -d proxy` was not run after adding it |
| `ssh -L` prints `channel N: open failed: administratively prohibited` | the host's sshd forbids forwarding (`AllowTcpForwarding no`, `DisableForwarding yes`, or `restrict` / `no-port-forwarding` on the key in `authorized_keys`). Check with `sudo sshd -T \| grep -i forwarding`, then allow it, e.g. `AllowTcpForwarding local` and `PermitOpen 127.0.0.1:8789` (add `127.0.0.1:8790` for the config editor) in `/etc/ssh/sshd_config.d/`, and reload sshd |
| The admin page says "Token refused" | `ADMIN_TOKEN` changed since the address was printed; print it again with `judge-cli admin-url` |
| The admin tools are missing from `/mcp` | the client sends `MCP_TOKEN`; the admin tools need `MCP_ADMIN_TOKEN` |
| `judgebot` exits naming `--web` and `index.html` | `--web` with no built page at `WEB_DIST`; drop `--web` or rebuild the image |
| The page 404s but `/api/health` is fine | `--web` is not in `JUDGE_ROLES`; the `judgebot roles` startup line lists what is on and what is off |
| `judgebot` restarts with `DISCORD_TOKEN is not set` on a deployment without Discord (often right after the upgrade to one service) | the default roles include `--discord`; add `JUDGE_ROLES='--api --web --jobs'` to `.env` and `docker compose up -d` |
| `judgebot` restarts with `API_INTERFACES: no longer read` | replace it in `.env` with the `JUDGE_ROLES` line the error names |
| Every Discord question is answered twice | a second process uses the same token without the gateway lease: the old `judgebot-bot` container left by an upgrade without `--remove-orphans` (§8; `docker rm -f judgebot-bot judgebot-api`), or a `judgebot` on another host with a database of its own |
| The bot is offline and the log says `standing by: another instance holds the Discord gateway` | another `judgebot --discord` on the same database holds the gateway lease; the line names its pid and `application_name` (§8, Gateway holder). Stop that process (`docker rm -f` a leftover container), and this one connects 15 seconds later |
| `scripts/deploy.sh` stops with `2 judgebot containers are running` | an earlier deploy was interrupted between starting the new container and removing the old one. `docker compose ps judgebot` lists both; `docker stop` the older one (it drains first) and `docker rm` it |
| `scripts/deploy.sh` removed the new container | it was not healthy in time or it restarted; the script printed its last log lines (a failed migration, a missing variable). The old container kept serving |
| `localhost:8787` answers 502 while the tunnel works | the `proxy` service is up and `judgebot` runs no network role, or is restarting (`docker compose ps`) |
| `up -d` fails with `port is already allocated` for 8787 | something else on the host listens on 8787: a `judgebot` from `cargo run`, or a container from before the `proxy` service that is not part of this compose project |
| `judgebot` reads healthy but nothing answers on 8787 | the roles have no `--api`, `--web` or `--mcp`, so the healthcheck passes without probing (§4) |
| Everyone shares one rate-limit bucket | `API_CLIENT_IP=peer` behind the public site, a proxy or the tunnel, where every request comes from the proxy; or `proxy` with the tunnel, or `cloudflare` with the public site (§3, Client address) |
| Rate limiting never triggers | `API_CLIENT_IP=cloudflare` while something other than Cloudflare can reach the origin, so `CF-Connecting-IP` is caller-supplied; or `proxy` while a route that does not remove `X-Judgebot-Client-IP` is public (a tunnel to `judgebot:8787`, or 8788 published beyond loopback) |
| `judgebot` restart-loops naming `JUDGE_OPERATOR_DISCORD` / `JUDGE_OPERATOR_EMAIL` | the contact a role must show is unset or malformed in `.env` (`--discord` needs the first, `--api`/`--web`/`--mcp` the second); set it and `docker compose up -d` |
| `cloudflared` restart-loops on startup | `COMPOSE_PROFILES=tunnel` with `TUNNEL_TOKEN` empty or stale in `.env.deploy` |
| Members are told the bot "hit its spending cap" | `JUDGE_MAX_USD` is spent for the process or the period (`judge-cli stats` shows the days); raise it and `docker compose up -d`, or wait for the period to turn |
| `embedding refused by the spend cap`, or a refresh's `embed` step failing with `stopped at the spend cap` | The cap covers embeddings too. Questions are answered without vector search meanwhile. Raise `JUDGE_MAX_USD` or wait for the period, then `scripts/refresh-data.sh embed` fills the vectors it skipped |
| A refresh or backup failed and nobody noticed | set `JUDGE_ALERT_WEBHOOK` in `.env`, and in `.env.deploy` for the `backup` service, which reads only that file (and `.env`'s `JUDGE_LOCALE`, the language of its alerts, which Compose passes it). A scheduled refresh and the service post there on the first failure of a streak and on recovery; both scripts post on a non-zero exit |
| When was the data last refreshed? | `/help`, the web footer or `GET /api/about` (`freshness`) for the age of the last success; `judge-cli stats` for the last five runs (§7) |
| The data is days old and `refresh_runs` has no recent row | `JUDGE_REFRESH_HOURS=0` with no cron entry, or the `judgebot` log says `scheduled refresh paused` (a container on an older image than the schema, migrations pending with `JUDGE_AUTO_MIGRATE=false`, or no rules loaded yet: `docker compose run --rm refresh init`). `docker compose logs judgebot \| grep 'scheduled'` shows which |
| `refresh step skipped` for `embed`, `rows to embed, over the 800` | the spend guard: a scheduled run found more empty vectors than a CR release leaves. Run `scripts/refresh-data.sh embed` if that spend is expected (§7) |
| Backup cron silently never runs | log path not writable by your user, or `.env.deploy` missing |
| `docker compose up -d` stops at `env file .env.deploy not found` | the `backup` or `tunnel` profile is in `COMPOSE_PROFILES` and `.env.deploy` does not exist (`cp .env.deploy.example .env.deploy`) |
| `backup` keeps restarting | a setting in `.env.deploy` is missing or malformed; `docker compose logs backup` names each one (`R2_*`, `BACKUP_*`, `JUDGE_ALERT_WEBHOOK`) |
| The backup alert says `pg_dump` is older than the server | `db` runs a newer PostgreSQL major than the image's `pg_dump` (16); the image needs a newer client (§6) |
| No backup in the bucket for over a week, and no alert | the `backup` profile is not in `COMPOSE_PROFILES` (`docker compose ps backup` lists nothing) and there is no cron entry; or `JUDGE_ALERT_WEBHOOK` is not in `.env.deploy` |
| Refresh logs `waiting for it to finish` and sits there | another refresh or ingest step holds the refresh lease (a scheduled or cron run and a manual one overlapped); it proceeds when that one ends, or fails after an hour. The log line names the holder; `select pid, application_name, query_start, state from pg_stat_activity where application_name like 'judgebot refresh lease%';` lists it (`… since <UTC time>`) and any waiters (`… waiting`, waiting since `query_start`). A hung holder can be ended with `select pg_terminate_backend(<pid>);`. A killed run's lock is dropped by Postgres, so there is nothing to remove |
| Refresh loads the CR on every run | `rules.cr_version` disagrees with the file name on Wizards' page — check the `current comprehensive rules release` log line for `published` vs `stored` |
| Refresh runs but the bot still cites the old CR | it does not: retrieval reads the database live; check the run actually finished (`refresh step ok` for `rules` and `embed`) |
| `JUDGE_CONFIG=/etc/judgebot/judge.toml: file not found` at startup | `JUDGE_CONFIG` in `.env` names a host file that does not exist; Docker mounted an empty directory in its place (and created a root-owned one on the host — `sudo rmdir` it) |
| `providers.X: NAME (api_key_env) is not set` at startup | the key was exported in the shell that ran `cargo run` but never written to `.env`, which is all the containers read; or, from `refresh` alone, only the embed provider's key was set because "refresh only embeds" — `refresh` resolves the chat stages too, so the extract/synth providers' keys must be in `.env` as well |
| Edited `judge.toml`, `docker compose up -d`, nothing changed | `up -d` recreates only on a configuration or image change and a bind-mounted file's content is neither; `docker compose restart judgebot` (§4) |
| `providers.X (...): no credentials` at startup | a cloud endpoint with an empty chain: no `AWS_*` in `.env`, no mounted credentials file, or a mounted file the `nobody` user cannot read |
| `embedding space mismatch; vector search off` | `[models.embed]` names a model or width other than the one the database holds; `reembed` (§7) to move the data, or change the file back |
| `no price for X/Y` at startup | a model on an `openai` provider without `[models.<stage>.pricing]`; add one (USD per million tokens) or `pricing = "free"` on the provider |
