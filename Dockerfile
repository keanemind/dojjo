# Dojjo sync-server for Dokku / Docker.
# Build context: repository root (`docker build -f Dockerfile .`).

FROM rust:1-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install sqlx-cli --no-default-features --features rustls,sqlite --locked

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY dojjo-mirror/Cargo.toml dojjo-mirror/
COPY tus-server/Cargo.toml tus-server/
COPY sync-server/Cargo.toml sync-server/
COPY client/Cargo.toml client/

COPY dojjo-mirror dojjo-mirror
COPY tus-server tus-server
COPY sync-server sync-server
COPY client client
COPY scripts scripts

ENV DATABASE_URL=sqlite:///app/sync-server/data.db
RUN sqlx database create \
    && sqlx migrate run --source sync-server/migrations

RUN cargo build --release -p sync-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/sync-server /usr/local/bin/sync-server

ENV DOJJO_DATA_DIR=/data
ENV DOJJO_LISTEN=0.0.0.0:3000
ENV DATABASE_URL=sqlite:///data/data.db
# Set DOJJO_PUBLIC_URL to the URL clients use (e.g. http://host:3000). See docs/SYNC_SERVER_DEPLOY.md.

EXPOSE 3000

# Dokku `storage:mount` should bind a host directory here before you rely on persistence.
VOLUME ["/data"]

CMD ["sync-server"]
