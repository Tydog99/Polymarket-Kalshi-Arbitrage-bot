#!/bin/bash
set -e

# Copy static assets from image to volume (if not already present)
if [ ! -f /data/kalshi_team_cache.json ]; then
    echo "Copying team cache to volume..."
    cp /usr/local/share/arb/kalshi_team_cache.json /data/kalshi_team_cache.json
fi

# Decode Kalshi private key from base64 secret to persistent volume
if [ -n "$KALSHI_KEY_B64" ]; then
    echo "$KALSHI_KEY_B64" | base64 -d > /data/kalshi.pem
    chmod 600 /data/kalshi.pem
fi

# Write env vars to profile so SSH sessions inherit them
cat > /etc/profile.d/arb-env.sh << 'EOF'
export KALSHI_PRIVATE_KEY_PATH="/data/kalshi.pem"
export KALSHI_PRIVATE_KEY=$(echo "$KALSHI_KEY_B64" | base64 -d)
alias execute-controler="CAPTURE_DIR=./.captures CAPTURE_FILTER=all CONTROLLER_PLATFORMS=kalshi,polymarket DRY_RUN=0 controller --controller-platforms=kalshi,polymarket"
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
