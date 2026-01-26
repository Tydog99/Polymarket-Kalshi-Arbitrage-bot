#!/bin/bash
set -e

# Decode Kalshi private key from base64 secret to persistent volume
if [ -n "$KALSHI_KEY_B64" ]; then
    echo "$KALSHI_KEY_B64" | base64 -d > /data/kalshi.pem
    chmod 600 /data/kalshi.pem
fi

# Write env vars to profile so SSH sessions inherit them
cat > /etc/profile.d/arb-env.sh << 'EOF'
export KALSHI_PRIVATE_KEY_PATH="/data/kalshi.pem"
export KALSHI_PRIVATE_KEY=$(echo "$KALSHI_KEY_B64" | base64 -d)
EOF

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
