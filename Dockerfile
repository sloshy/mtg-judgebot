# Build with the committed .sqlx offline data; no DB needed at compile time.
FROM node:26-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# Pinned to bookworm to match the debian:bookworm-slim runtime below: the bare
# `rust:1.99-slim` tag moved to trixie (glibc 2.41), and a binary linked there
# fails on bookworm (glibc 2.36) with `version `GLIBC_2.38' not found`.
#
# The Rust build is split with cargo-chef so that dependencies (about two thirds
# of a cold build's CPU) sit in their own layer, keyed on the manifests and
# Cargo.lock only. A plain `COPY . .` + `cargo build` invalidated that layer on
# every source change, so every CI run compiled ~350 crates from scratch.
FROM rust:1.99-slim-bookworm AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app
ENV SQLX_OFFLINE=true

# The recipe changes only when a manifest or the lockfile does, so the cook
# layer below stays a cache hit across ordinary commits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Packages, features and profile must be the build's own, or cargo recompiles
# the dependencies with different flags. The Anthropic cloud endpoints (Claude
# Platform on AWS, Bedrock, Vertex) are judge-bot's default features; named
# here so the image keeps them if the default ever changes (judge-bot is a
# workspace member, so they can be named without building it on its own).
#
# The `touch` gives every cooked artifact one timestamp. Cargo compiles a crate
# as soon as its dependency's metadata exists, so the dependency's library can
# finish *after* its dependent; the next cargo run reads that as "dependency
# newer than we are" and rebuilds a cascade (62 of ~350 crates, including
# aws-lc-sys and sqlx), which cost as much as the cook saved. Equal times are not
# stale, and the time is taken after the registry sources were unpacked, so
# those still read as older. The workspace crates rebuild regardless: the recipe
# cooks them under a placeholder version, a different unit.
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
    -p judgebot -p judge-agent -p judge-configure --features judge-bot/aws,judge-bot/gcp \
    && find target -exec touch -h -d "@$(date +%s)" {} +
COPY . .
# The commit the image is built from, for the source offer every remote
# interface makes (`GET /api/about`, `/license`, the MCP instructions): the
# context has no `.git` (.dockerignore), so crates/bot/build.rs (judge-bot's,
# which every binary that makes the offer links) cannot ask git
# and reads JUDGE_COMMIT instead, with JUDGE_DIRTY=1 when the tree copied in
# does not match that commit. CI passes github.sha from a clean checkout; a
# local `docker compose build` passes both from the environment (blank commit
# = "commit unknown", which the offer says rather than guessing; a commit
# that is not a hash fails the build). Declared after the cook so a new
# commit never invalidates the dependency layer.
ARG JUDGE_COMMIT=
ARG JUDGE_DIRTY=
ENV JUDGE_COMMIT=$JUDGE_COMMIT JUDGE_DIRTY=$JUDGE_DIRTY
# The binaries are copied out and `target/` removed in the same step, so this
# layer holds megabytes rather than the ~4 GB target directory, which CI would
# otherwise export to its build cache on every run and never read back.
RUN cargo build --release -p judgebot -p judge-agent -p judge-configure --features judge-bot/aws,judge-bot/gcp \
    && mkdir /out \
    && cp target/release/judgebot target/release/judge-cli target/release/judge-mcp target/release/judge-config /out/ \
    && rm -rf target

# pg_dump for `judgebot backup` (the `backup` compose service), from the
# PostgreSQL project's own Debian repository (PGDG): bookworm's own client is
# 15, and pg_dump refuses a server newer than itself. PG_MAJOR is the major of
# docker-compose.yml's `db` image (pgvector/pgvector:pg16); `scripts/check.sh
# lint` fails when the two differ.
# The repository's signing key is pinned by checksum, so a key swapped on
# the server (or in transit) fails the build instead of being trusted. It is
# the key with fingerprint B97B 0AFC AA1A 47F0 44F2 44A0 7FCC 7D46 ACCC 4CF8,
# which has no expiry date; if PGDG ever re-issues the file, check the new
# one's fingerprint (`gpg --show-keys`) and update the sum. `--checksum` needs
# BuildKit, which `docker build` uses by default and CI's buildx provides.
FROM debian:bookworm-slim AS pgdg
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
ADD --checksum=sha256:0144068502a1eddd2a0280ede10ef607d1ec592ce819940991203941564e8e76 \
    https://www.postgresql.org/media/keys/ACCC4CF8.asc /etc/apt/keyrings/pgdg.asc
RUN chmod 644 /etc/apt/keyrings/pgdg.asc \
    && echo "deb [signed-by=/etc/apt/keyrings/pgdg.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list

# The whole client package depends on Perl (postgresql-client-common's
# wrapper), about 85 MB the backup never runs. Only the pg_dump binary is
# taken from this stage; the runtime installs the libraries it links
# (libpq5 from the same repository) and checks that it runs.
FROM pgdg AS pgdump
ARG PG_MAJOR=16
RUN apt-get update && apt-get install -y --no-install-recommends postgresql-client-${PG_MAJOR} \
    && rm -rf /var/lib/apt/lists/* \
    && cp /usr/lib/postgresql/${PG_MAJOR}/bin/pg_dump /pg_dump

FROM pgdg
# The ingest cache is owned by the runtime user so that a named volume mounted
# there (compose service `refresh`) inherits writable ownership on first use.
RUN apt-get update && apt-get install -y --no-install-recommends libpq5 liblz4-1 libzstd1 && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/cache/judgebot && chown nobody /var/cache/judgebot
# `pg_dump --version` fails the build when a library it links is missing, on
# each platform the image is built for.
COPY --from=pgdump /pg_dump /usr/local/bin/pg_dump
RUN pg_dump --version
# The one long-running binary: its roles (`--discord --api --web --mcp
# --jobs`) are launch options, `judgebot ingest` is the data command line and
# `judgebot backup` the database backup.
COPY --from=builder /out/judgebot /usr/local/bin/judgebot
# The agent surface: `judge-cli` (one subcommand per operation, JSON out) and
# `judge-mcp` (the MCP server over stdio). The same tools are served over HTTP
# by `judgebot --mcp` at /mcp (MCP_TOKEN); these two are for a shell on the
# host (`docker compose run --rm --entrypoint judge-cli judgebot ...`).
COPY --from=builder /out/judge-cli /usr/local/bin/judge-cli
COPY --from=builder /out/judge-mcp /usr/local/bin/judge-mcp
# The config editor, a page on localhost for judge.toml and .env; run with the
# checkout mounted (site: self-hosting/config-editor).
COPY --from=builder /out/judge-config /usr/local/bin/judge-config
ENV INGEST_CACHE_DIR=/var/cache/judgebot
COPY --from=web /web/dist /srv/web
ENV WEB_DIST=/srv/web
USER nobody
# With no arguments it runs the roles in JUDGE_ROLES.
ENTRYPOINT ["judgebot"]
