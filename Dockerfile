# Build with the committed .sqlx offline data; no DB needed at compile time.
FROM node:22-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1.97-slim AS builder
WORKDIR /app
COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release -p judge-bot -p judge-api -p judge-ingest -p judge-agent

FROM debian:bookworm-slim
# The ingest cache is owned by the runtime user so that a named volume mounted
# there (compose service `refresh`) inherits writable ownership on first use.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/cache/judgebot && chown nobody /var/cache/judgebot
COPY --from=builder /app/target/release/bot /usr/local/bin/judge-bot
COPY --from=builder /app/target/release/api /usr/local/bin/judge-api
# Third entrypoint: the scheduled data refresh (`judge-ingest refresh`).
COPY --from=builder /app/target/release/ingest /usr/local/bin/judge-ingest
# The agent surface: `judge-cli` (one subcommand per operation, JSON out) and
# `judge-mcp` (the MCP server over stdio). The same tools are served over HTTP
# by judge-api at /mcp when MCP_TOKEN is set; these two are for a shell on the
# host (`docker compose run --rm --entrypoint judge-cli api ...`).
COPY --from=builder /app/target/release/judge-cli /usr/local/bin/judge-cli
COPY --from=builder /app/target/release/judge-mcp /usr/local/bin/judge-mcp
ENV INGEST_CACHE_DIR=/var/cache/judgebot
COPY --from=web /web/dist /srv/web
ENV WEB_DIST=/srv/web
USER nobody
# The api service overrides this with judge-api.
ENTRYPOINT ["judge-bot"]
