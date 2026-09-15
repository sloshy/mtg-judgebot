# Changelog

Notable changes an operator or user would notice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project is pre-1.0, so
minor versions may change configuration or schema (migrations apply automatically
unless `JUDGE_AUTO_MIGRATE=false`).

## [Unreleased]

### Added
- Discord `/help` (what the bot does, how to ask, what it stores) and `/forget`, which
  deletes the caller's ratings, the only per-user data kept.
- `GET /api/health` now checks the database and answers 503 when it is unreachable; the
  compose file gives `api` a healthcheck, starts the tunnel only once it passes, and
  caps each long-running service's log at 30 MB (`refresh` is a one-shot whose log dies
  with it).
- The web page carries a favicon, a description and Open Graph tags, and its footer
  names the Fan Content Policy, Scryfall and the source repository.
- `docs/DECISIONS.md`: every load-bearing design decision (D1–D17) with the alternative
  it rejected, in place of the retired language and tenancy proposals; the provider
  proposal became the reference `docs/PROVIDERS.md`, describing what was built.
- `DB_PORT` in `.env` moves the port compose publishes Postgres on (default 5432).
- `NOTICE`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue and pull
  request templates, and this changelog, for the public release.
- A CI workflow (`ci.yml`) running rustfmt, clippy, the offline sqlx build, a `.sqlx`
  freshness check, the test suite against pgvector Postgres, and the web build. The
  image is now published only from a tree that passes it.
- `JUDGE_IMAGE` in `.env` names the container image; the compose file defaults to the
  upstream package, so a fork points it at its own.
- `rust-toolchain.toml` pinning Rust 1.97 with rustfmt and clippy.

### Changed
- The Discord bot token and the MCP bearer token are held as redacted secrets; a
  `Debug` rendering of either configuration prints `<redacted>`.
- The extractor's "no category" warning no longer includes the question text.
- The whole workspace is formatted with rustfmt.
- Compose publishes Postgres on loopback **5432**; `.env.example` and CI use the same
  port. An existing `.env` pointing at 5433 keeps working with `DB_PORT=5433`.
- The documentation is organised around running your own judgebot: the Discord setup
  page links into Discord's own documentation for each step, and the maintainer's
  instance is described as private to their servers rather than as a bot to invite.
- `docs/DEPLOYMENT.md` uses placeholder hostnames and paths instead of the upstream
  operator's.

### Removed
- `API_TRUST_FORWARDED`. Setting it is a startup error; see `SECURITY.md` for why
  `API_CLIENT_IP` replaced it.

## Earlier

Everything before this file is in the git history. The pipeline, providers
(`judge.toml`), agent sessions, MCP transport, web page, call retirement, rule
renumbering and embedding-space tracking all landed there; `docs/ARCHITECTURE.md`
describes them as they stand.
