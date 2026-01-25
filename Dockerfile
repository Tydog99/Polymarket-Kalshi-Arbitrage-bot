FROM rust:1.75-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p controller

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install Tailscale
RUN curl -fsSL https://tailscale.com/install.sh | sh

# Copy binary
COPY --from=builder /app/target/release/controller /usr/local/bin/controller

# Volume mount point for persistent data
RUN mkdir -p /data
WORKDIR /data

# Startup script
COPY fly-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/fly-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/fly-entrypoint.sh"]
