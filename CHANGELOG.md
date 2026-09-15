# Changelog

Notable changes an operator or user would notice. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project is pre-1.0, so
minor versions may change configuration or schema (migrations apply automatically
unless `JUDGE_AUTO_MIGRATE=false`).

## [Unreleased]

### Added
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
