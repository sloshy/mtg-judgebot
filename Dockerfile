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
RUN cargo build --release -p judge-bot -p judge-api

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/bot /usr/local/bin/judge-bot
COPY --from=builder /app/target/release/api /usr/local/bin/judge-api
COPY --from=web /web/dist /srv/web
ENV WEB_DIST=/srv/web
USER nobody
# The api service overrides this with judge-api.
ENTRYPOINT ["judge-bot"]
