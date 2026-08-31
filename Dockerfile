# Build with the committed .sqlx offline data; no DB needed at compile time.
FROM rust:1.97-slim AS builder
WORKDIR /app
COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release -p judge-bot

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/bot /usr/local/bin/judge-bot
USER nobody
ENTRYPOINT ["judge-bot"]
