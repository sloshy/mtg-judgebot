# Build with the committed .sqlx offline data; no DB needed at compile time.
FROM node:24-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# Pinned to bookworm to match the debian:bookworm-slim runtime below: the bare
# `rust:1.98-slim` tag moved to trixie (glibc 2.41), and a binary linked there
# fails on bookworm (glibc 2.36) with `version `GLIBC_2.38' not found`.
#
# The Rust build is split with cargo-chef so that dependencies (about two thirds
# of a cold build's CPU) sit in their own layer, keyed on the manifests and
# Cargo.lock only. A plain `COPY . .` + `cargo build` invalidated that layer on
# every source change, so every CI run compiled ~350 crates from scratch.
FROM rust:1.98-slim-bookworm AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app
ENV SQLX_OFFLINE=true

# The recipe changes only when a manifest or the lockfile does, so the cook
# layer below stays a cache hit across ordinary commits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Packages, features and profile must be the build's own, or cargo recompiles
# the dependencies with different flags. The Anthropic cloud doors (Claude
# Platform on AWS, Bedrock, Vertex) are judge-bot's default features; named
# here so the image keeps them if the default ever changes.
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
    -p judge-bot -p judge-api -p judge-ingest -p judge-agent --features judge-bot/aws,judge-bot/gcp \
    && find target -exec touch -h -d "@$(date +%s)" {} +
COPY . .
# The commit the image is built from, for the source offer every remote
# interface makes (`GET /api/about`, `/license`, the MCP instructions): the
# context has no `.git` (.dockerignore), so crates/bot/build.rs cannot ask git
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
RUN cargo build --release -p judge-bot -p judge-api -p judge-ingest -p judge-agent --features judge-bot/aws,judge-bot/gcp \
    && mkdir /out \
    && cp target/release/bot target/release/api target/release/ingest target/release/judge-cli target/release/judge-mcp /out/ \
    && rm -rf target

FROM debian:bookworm-slim
# The ingest cache is owned by the runtime user so that a named volume mounted
# there (compose service `refresh`) inherits writable ownership on first use.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/cache/judgebot && chown nobody /var/cache/judgebot
COPY --from=builder /out/bot /usr/local/bin/judge-bot
COPY --from=builder /out/api /usr/local/bin/judge-api
# Third entrypoint: the scheduled data refresh (`judge-ingest refresh`).
COPY --from=builder /out/ingest /usr/local/bin/judge-ingest
# The agent surface: `judge-cli` (one subcommand per operation, JSON out) and
# `judge-mcp` (the MCP server over stdio). The same tools are served over HTTP
# by judge-api at /mcp when MCP_TOKEN is set; these two are for a shell on the
# host (`docker compose run --rm --entrypoint judge-cli api ...`).
COPY --from=builder /out/judge-cli /usr/local/bin/judge-cli
COPY --from=builder /out/judge-mcp /usr/local/bin/judge-mcp
ENV INGEST_CACHE_DIR=/var/cache/judgebot
COPY --from=web /web/dist /srv/web
ENV WEB_DIST=/srv/web
USER nobody
# The api service overrides this with judge-api.
ENTRYPOINT ["judge-bot"]
