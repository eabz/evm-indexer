# syntax=docker/dockerfile:1

# Builder and runtime are pinned to the same Debian release (bookworm) so the
# binary links against the glibc / OpenSSL versions that exist at runtime.
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# ---- planner: compute the dependency recipe -------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY bin ./bin
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder: cook dependencies (cached layer), then build the binary ------
# No extra system packages are needed here:
#   - hypersync-net-types ships pre-generated Cap'n Proto code (no build.rs),
#     so the `capnp` compiler is not required.
#   - libssl-dev / pkg-config (for the clickhouse crate's native-tls) are
#     already part of the rust base image.
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# build.rs embeds migrations/*.sql into the binary (see src/db/migrate.rs).
COPY Cargo.toml Cargo.lock build.rs ./
COPY bin ./bin
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked --bin indexer

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 indexer \
    && useradd --system --uid 10001 --gid indexer --no-create-home --shell /usr/sbin/nologin indexer

COPY --from=builder /app/target/release/indexer /usr/local/bin/indexer

USER indexer

# All flags can also be supplied as environment variables (see README.md).
ENTRYPOINT ["indexer"]
