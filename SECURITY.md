# Security

## Reporting a vulnerability

Report security problems privately, not in a public issue. Use GitHub's
**Report a vulnerability** button on the repository's Security tab (private
vulnerability reporting). If that is unavailable, contact the maintainer at the address
on the project's commits. You should hear back within a week.

In scope: the bot, the HTTP API and web app, the MCP transport, the admin interface, the
ingest tooling,
and the deployment files in this repository. Out of scope: the third-party services the
bot talks to (Discord, Scryfall, model providers, Cloudflare).

## Deployment assumptions

Operators should know these properties. The reasoning behind each is in
`docs/DEPLOYMENT.md` and the comments in `.env.example`.

- **Spend is the main asset to protect.** Every model call, embeddings included, goes
  through a hard cap (`JUDGE_MAX_USD`) that reserves the worst case before sending. The anonymous API
  adds a per-IP fixed-window rate limit ahead of the concurrency semaphore, and the
  runbook puts a Cloudflare rate-limiting rule in front of that.
- **Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP`
  is `peer` or `cloudflare` (`CF-Connecting-IP`). `X-Forwarded-For` is never read,
  because Cloudflare appends to a caller-supplied header instead of replacing it,
  which would give every request a fresh allowance. `cloudflare` is only sound when
  nothing but Cloudflare can reach the origin.
- **The MCP endpoint is bearer-token only and off by default.** It mounts only when
  `judgebot` is launched with `--mcp` *and* `MCP_TOKEN` (at least 24 bytes) is set.
  The flag alone is refused at startup, and the token alone serves nothing. The token is
  compared in constant time. Its own `judge` quota (`MCP_JUDGE_LIMIT`) bounds what a
  leaked token can spend. `MCP_ALLOWED_HOSTS` must name the public hostname. To rotate
  the token, change the variable and recreate `judgebot` (`docker compose up -d`).
- **The operator's reads have their own credentials and their own listener.** Private
  calls, askers' Discord ids, raters and failed questions are readable only by the
  operator (D37):
  - The admin pages (`--admin`) listen on `ADMIN_ADDR` (loopback; in compose,
    `127.0.0.1:${ADMIN_PORT:-8789}` on the host through the proxy) behind `ADMIN_TOKEN`. The public
    listener answers their paths 404, and the tunnel never points at them. cloudflared
    shares the compose network and could reach the port, so the token is the guard
    there.
  - `/mcp` offers the admin tools only to `MCP_ADMIN_TOKEN` (off unless set). `MCP_TOKEN`
    reaches a handler whose type has no admin tools. Which handler a request reaches is
    one constant-time comparison of its bearer token against each, covered by a test.
  - Each token is at least 24 bytes and compared in constant time, and no two of
    `MCP_TOKEN`, `MCP_ADMIN_TOKEN` and `ADMIN_TOKEN` may be equal.
  - Stdio `judge-mcp` and `judge-cli` hold the admin reads too: whoever runs them already
    holds `DATABASE_URL`. So does any program built on `judge_agent::Toolbox::from_env`,
    which connects with it.
- **Secrets never live in tracked files.** `judge.toml` names environment variables.
  `.env` holds the keys `judgebot` reads. `.env.deploy` holds the tunnel and backup
  credentials that only `cloudflared`, the `backup` service and the backup script read;
  the `backup` service reads nothing of `.env`. Every credential type
  in the code redacts itself in `Debug`.
- **Postgres, the API and the admin interface bind to loopback** in `docker-compose.yml`. The compose
  database uses a default password (`judgebot`), which is fine only while that binding
  holds. Change it if you publish the port.
- **The image runs as `nobody`** and is rebuilt by CI from the committed lockfiles.
- **User data stored:**
  - the question and answer text of every call, private ones included
  - the Discord thread or web session id it was asked in, and the interface
  - for a Discord question, the asker's user id and username at asking time
  - the Discord user id of anyone who presses a rating button, the score, and for a
    *Partially correct* or *Incorrect* rating the reason they give in its form (a
    category and up to two texts of 1000 characters), which the operator reads in the
    feedback queue (`feedback_queue`, with the operator's note on it)
  - for a rating given on the web: the browser tab's session id (the one its questions
    share history under), the reason, the name the rater gave, and the address and user
    agent it came from. Address and user agent are cleared once the rating is
    `FEEDBACK_IP_DAYS` old (default 90, at most 3650, never kept for good) by every
    rating written and by each refresh's `sweep` step; the rating and its reason stay.
  - for a call that failed (`failed_calls`): the question, the error, the model's answer
    text and, for a Discord question, the asker's user id and username. Pruned to 30
    days / 500 rows when a later failure is recorded.

  Calls are kept until the operator's `JUDGE_CALL_RETENTION_DAYS` (unset: kept), then
  deleted with their ratings by the next refresh.

  `/forget` deletes the user's ratings with their reasons and feedback-queue items (the
  operator's notes on them included), and removes their id and username from their calls
  and failed calls. A private call loses its question, answer, citations and card ids too,
  and a failed one its text. A public call keeps its text, which the channel saw.
  - for each run of the pipeline (`judge_runs`): the interface, whether it was private,
    how it ended, when, and the models, tokens and cost of each request. No text and no
    user id.
  - for each "did you mean?" prompt a card is picked from (`pick_claims`): the Discord
    message id, when the prompt was shown and a short digest of its content. No text and
    no user id; rows are deleted after a day.

  The bot requests no gateway intents, so it never receives message content beyond the
  slash-command input. Process logs at `info` record each rating with the user id and its
  reason's category (never the reason's text). The compose file rotates them at 30 MB per
  container. Shipping or dropping them is up to the operator.
