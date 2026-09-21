# syntax=docker/dockerfile:1

# Builder and runtime are pinned to the same Debian release (bookworm) so the
# binary links against the glibc / OpenSSL versions that exist at runtime.
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# The build context is an allow-list, see .dockerignore: only Cargo.toml,
# Cargo.lock, build.rs, bin/, src/ and migrations/ are sent to the daemon.

# ---- planner: compute the dependency recipe -------------------------------
# `cargo chef prepare` only reads the manifests and the target layout (it has
# to see build.rs to know the package has a build script); it never runs
# build.rs, so migrations/ is not needed in this stage.
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

# build.rs embeds migrations/*.sql into the binary (see src/db/migrate.rs)
# and fails the build on a malformed migration set, so the directory must be
# present here. The image needs no SQL files at runtime.
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

# Documentation only: the port used for --metrics-addr / METRICS_ADDR in
# docker-compose.yml (/metrics, /healthz, /readyz). Metrics are off unless
# that option is set, which is why there is no HEALTHCHECK here; compose
# defines one (it uses bash's /dev/tcp, the image ships no curl or wget).
EXPOSE 9090

# `indexer` without a subcommand is `indexer run`; `indexer migrate` and
# `indexer verify` are the other subcommands.
# All flags can also be supplied as environment variables (see README.md).
ENTRYPOINT ["indexer"]
