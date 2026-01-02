# ============================================
# Stage 1: Chef - Prepare build environment
# ============================================
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef

USER root
WORKDIR /app

# ============================================
# Stage 2: Planner - Analyze dependencies
# ============================================
FROM chef AS planner

WORKDIR /app

# Only copy files needed for dependency analysis
# This ensures recipe.json only changes when dependencies change
COPY Cargo.toml Cargo.lock ./

# Create dummy binary to satisfy cargo (matches [[bin]] path in Cargo.toml)
RUN mkdir -p bin && echo "fn main() {}" > bin/indexer.rs

RUN cargo chef prepare --recipe-path recipe.json

# ============================================
# Stage 3: Builder - Build the application
# ============================================
FROM chef AS builder

WORKDIR /app

# Copy dependency recipe and build deps first (cached layer)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy source code and build application
COPY . .
RUN cargo build --release

# ============================================
# Stage 4: Runtime - Minimal production image
# ============================================
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/indexer /app/indexer

# Copy schema files for migrations
COPY --from=builder /app/schema /app/schema

# Set executable permissions
RUN chmod +x /app/indexer

# Run as non-root user for security
RUN useradd -r -u 1000 indexer
USER indexer

ENTRYPOINT ["/app/indexer"]