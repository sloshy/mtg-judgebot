# Security

## Reporting a vulnerability

Please report security problems privately rather than in a public issue. Use GitHub's
**Report a vulnerability** button on the repository's Security tab (private
vulnerability reporting). If that is unavailable, contact the maintainer directly using
the address on the project's commits. You should hear back within a week.

In scope: the bot, the HTTP API and web page, the MCP transport, the ingest tooling,
and the deployment files in this repository. Out of scope: the third-party services the
bot talks to (Discord, Scryfall, model providers, Cloudflare).

## What the deployment assumes

Operators should know these properties; the reasoning behind each is in
`docs/DEPLOYMENT.md` and the comments in `.env.example`.

- **Spend is the asset most worth protecting.** Every model call goes through a hard
  cap (`JUDGE_MAX_USD`) that reserves the worst case before sending. The anonymous API
  adds a per-IP fixed-window rate limit ahead of the concurrency semaphore, and the
  runbook puts a Cloudflare rate-limiting rule in front of that.
- **Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP`
  is `peer` or `cloudflare` (`CF-Connecting-IP`). `X-Forwarded-For` is never read,
  because Cloudflare appends to a caller-supplied header instead of replacing it,
  which would give every request a fresh allowance. `cloudflare` is only sound when
  nothing but Cloudflare can reach the origin. The old `API_TRUST_FORWARDED` knob is
  rejected at startup for this reason.
- **The MCP endpoint is bearer-token only and off by default.** It mounts only when
  `MCP_TOKEN` (at least 24 bytes) is set, compares in constant time, and applies its
  own `judge` quota (`MCP_JUDGE_LIMIT`) as the blast radius of a leaked token.
  `MCP_ALLOWED_HOSTS` must name the public hostname. Rotate the token by changing the
  variable and restarting `api`.
- **Secrets never live in tracked files.** `judge.toml` names environment variables;
  `.env` holds the keys the bot and API read, `.env.deploy` the tunnel and backup
  credentials that only `cloudflared` and the backup script read. Every credential type
  in the code redacts itself in `Debug`.
- **Postgres and the API bind to loopback** in `docker-compose.yml`. The compose
  database uses a default password (`judgebot`), which is fine only while that binding
  holds; change it if you publish the port.
- **The image runs as `nobody`** and is rebuilt by CI from the committed lockfiles.
- **User data stored:** the question and answer text of every call, the Discord
  thread or web session id it was asked in, and the Discord user id of anyone who
  presses a rating button (`/forget` deletes those). No message content beyond the
  slash-command input is ever received; the bot requests no gateway intents. Process
  logs at `info` record each rating with the user id; the compose file rotates them at
  30 MB per container, and they are the operator's to ship or drop.
