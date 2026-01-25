#!/bin/bash
set -e

# Decode Kalshi private key from base64 secret
if [ -n "$KALSHI_KEY_B64" ]; then
    echo "$KALSHI_KEY_B64" | base64 -d > /tmp/kalshi.pem
    chmod 600 /tmp/kalshi.pem
    export KALSHI_PRIVATE_KEY_PATH="/tmp/kalshi.pem"
fi

# Start Tailscale daemon
tailscaled --state=/data/tailscale.state &
sleep 2

# Authenticate with Tailscale if auth key provided
if [ -n "$TAILSCALE_AUTHKEY" ]; then
    tailscale up --authkey="${TAILSCALE_AUTHKEY}" --hostname=arb-controller
fi

echo "=========================================="
echo "Container ready!"
echo "SSH in with: fly ssh console"
echo "Then run:    controller"
echo "=========================================="

# Keep container alive, waiting for SSH
tail -f /dev/null
