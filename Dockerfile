FROM rust:latest AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies (cached unless Cargo.toml/Cargo.lock change)
RUN cargo chef cook --release --recipe-path recipe.json -p controller -p remote-trader
# Build applications
COPY . .
RUN cargo build --release -p controller -p remote-trader

FROM debian:sid-slim
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install Tailscale
RUN curl -fsSL https://tailscale.com/install.sh | sh

# Copy binaries
COPY --from=builder /app/target/release/controller /usr/local/bin/controller
COPY --from=builder /app/target/release/remote-trader /usr/local/bin/trader

# Volume mount point for persistent data
RUN mkdir -p /data
WORKDIR /data

# Copy static assets (will be copied to volume by entrypoint if needed)
RUN mkdir -p /usr/local/share/arb
COPY controller/kalshi_team_cache.json /usr/local/share/arb/kalshi_team_cache.json

# Startup script
COPY fly-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/fly-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/fly-entrypoint.sh"]
