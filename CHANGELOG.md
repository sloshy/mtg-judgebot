# Changelog

Notable changes an operator or user would notice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is pre-1.0, so
minor versions may change configuration or schema. Migrations apply automatically
unless `JUDGE_AUTO_MIGRATE=false`.

## [Unreleased]

### Added
- Every remote interface names who runs the instance, beside the source offer. `/help`
  and `/license` show the operator's Discord username. `GET /api/about` (new
  `operator_email` and `operator_discord` fields), the page footer, the MCP
  initialization instructions and `about` tool show the support address. Each shows the
  other contact too when it is set. `judge-cli about` shows whichever are set and
  requires neither. The bot does not start without `JUDGE_OPERATOR_DISCORD` (a Discord
  username), nor `judge-api` without `JUDGE_OPERATOR_EMAIL` (a support address). A value
  that is set and malformed is refused by every binary.
- The AGPL source offer on every remote interface. The web page's footer (from the new
  `GET /api/about`, served whatever doors are off), Discord `/help` and a new `/license`
  command, the MCP server's initialization instructions and a new `about` tool, and
  `judge-cli about` all state the licence (AGPL-3.0-or-later) and copyright. They name
  the repository holding the instance's source with the commit the binary was built
  from. CI stamps the image. A local build reads git and says when the tree was dirty.
  `JUDGE_SOURCE_URL` points the offer at a fork and must be an http(s) URL.
- `judge-api` takes one flag per front door: `--api` (`POST /api/judge`), `--web` (the
  built page) and `--mcp` (the MCP transport). `--help` prints them. `GET /api/health`
  is served whatever is switched off. The startup log names the interfaces that are on
  and the ones that are not, so a missing page reads as a decision rather than a bug.
  `API_INTERFACES` in `.env` is what the `api` container passes.
- The published image is a manifest list for `linux/amd64` and `linux/arm64`, each
  built on a runner of its own architecture, so an ARM host pulls the same tag.
- Versioned image tags: publishing a GitHub release `vX.Y.Z` tags the image already
  built for that commit as `X.Y.Z`, `X.Y` and (from 1.0) `X` without rebuilding it.
  `JUDGE_IMAGE_TAG` accepts them alongside `sha-<short>`.
- Discord `/help` (what the bot does, how to ask, what it stores) and `/forget`, which
  deletes the caller's ratings, the only per-user data kept.
- `GET /api/health` now checks the database and answers 503 when it is unreachable. The
  compose file gives `api` a healthcheck, starts the tunnel only once it passes, and
  caps each long-running service's log at 30 MB (`refresh` is a one-shot whose log dies
  with it).
- The web page carries a favicon, a description and Open Graph tags, and its footer
  names the Fan Content Policy, Scryfall and the source repository.
- `docs/DECISIONS.md`: the main design decisions (D1–D18), each with the alternative it
  rejected, in place of the retired language and tenancy proposals. The provider proposal became
  the reference `docs/PROVIDERS.md`, describing what was built.
- `DB_PORT` in `.env` moves the port compose publishes Postgres on (default 5432).
- `NOTICE`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue and pull
  request templates, and this changelog, for the public release.
- A CI workflow (`ci.yml`) running rustfmt, clippy, the offline sqlx build, a `.sqlx`
  freshness check, the test suite against pgvector Postgres, and the web build. The
  image is now published only from a tree that passes it.
- `JUDGE_IMAGE` in `.env` names the container image. The compose file defaults to the
  upstream package, so a fork points it at its own.
- `rust-toolchain.toml` pinning Rust 1.97 with rustfmt and clippy.

### Changed
- **The web page is opt-in.** `judge-api` used to serve it whenever the process was
  running, with no way to turn it off. It now serves the JSON API alone unless `--web`
  is given. The compose file passes `--api --web`, so a `docker compose` deployment is
  unchanged. A deployment that starts `judge-api` by hand must add the flag. A `--web`
  launch whose `WEB_DIST` holds no `index.html` is refused at startup instead of
  answering 404s.
- **`/mcp` needs `--mcp` as well as `MCP_TOKEN`.** `--mcp` with no token is a startup
  error, because it would be an anonymous route to the judge pipeline. A token with no
  `--mcp` serves nothing and logs a warning naming the fix, so an existing deployment
  upgrades without its web page going down. Set `API_INTERFACES=--api --web --mcp` in
  `.env` to get the endpoint back.
- The Discord bot token and the MCP bearer token are held as redacted secrets. A
  `Debug` rendering of either configuration prints `<redacted>`.
- The extractor's "no category" warning no longer includes the question text.
- The workspace is formatted with rustfmt.
- Compose publishes Postgres on loopback **5432**. `.env.example` and CI use the same
  port. An existing `.env` pointing at 5433 keeps working with `DB_PORT=5433`.
- The documentation is organised around running your own judgebot. The Discord setup
  page links into Discord's documentation for each step, and no instance is offered
  as a bot to invite or a page to try.
- No running instance is named anywhere in the documentation. The site's first page is
  "What the judge is" (what it does, why there is no bot to invite, what an instance
  stores, the limits of an AI answer) in place of the page describing a public one.
- The documentation site's header carries a **Docs** link into that first page, at every
  width, and its search box no longer shifts between the landing page and the docs.
- `docs/DEPLOYMENT.md` uses placeholder hostnames and paths instead of the upstream
  operator's.
- Outbound HTTPS (the model providers, the embedders, Scryfall, Wizards' rules page)
  verifies certificates against the system's trust store instead of a bundled root
  list. The image ships `ca-certificates`. A binary run outside the image needs the
  host's CA bundle, and a CA the host trusts (a corporate proxy) is now trusted too.
  Discord traffic still uses the bundled roots.
- Dependencies updated: Rust 1.98, reqwest 0.13, poise 0.7, tower-http 0.7, rmcp 3.4,
  Vite 8, TypeScript 6, Node 24 for the web build, and cloudflared 2026.9.1 in the
  compose file.

### Fixed
- Documentation site: a table wider than the text column scrolls inside itself again
  instead of pushing the page sideways on a phone (the provider, configuration,
  architecture, deployment and explainer pages).

### Removed
- `API_TRUST_FORWARDED`. Setting it is a startup error. `SECURITY.md` explains why
  `API_CLIENT_IP` replaced it.

## Earlier

Everything before this file is in the git history. The pipeline, providers
(`judge.toml`), agent sessions, MCP transport, web page, call retirement, rule
renumbering and embedding-space tracking all landed there. `docs/ARCHITECTURE.md`
describes them as they stand.
