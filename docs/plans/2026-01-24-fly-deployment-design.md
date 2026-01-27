# Fly.io Deployment Design

## Overview

Deploy the arbitrage controller to Fly.io with:
- Amsterdam region (close to Polymarket's eu-west-1)
- Tailscale for connecting to local trader
- Interactive SSH access for TUI
- Persistent volume for state

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Fly.io (Amsterdam)                   │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                    Fly Machine                        │  │
│  │  ┌─────────────┐    ┌─────────────┐                   │  │
│  │  │ Controller  │    │ tailscaled  │◄── Tailscale mesh │  │
│  │  │   (TUI)     │    └─────────────┘                   │  │
│  │  └──────┬──────┘                                      │  │
│  │         │                                             │  │
│  │         ▼                                             │  │
│  │  ┌─────────────┐                                      │  │
│  │  │ Fly Volume  │ ← positions.json, tailscale.state    │  │
│  │  └─────────────┘                                      │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
            │ Tailscale                │ WebSocket
            ▼                          ▼
┌──────────────────┐        ┌─────────────────────┐
│  Your Laptop     │        │ Kalshi / Polymarket │
│  (Trader binary) │        │      APIs           │
└──────────────────┘        └─────────────────────┘
```

## Storage Strategy

| Data | Storage | Reason |
|------|---------|--------|
| Kalshi PEM (base64) | Fly Secrets | Encrypted, easy rotation |
| API keys | Fly Secrets | Encrypted at rest |
| positions.json | Fly Volume | Persists across deploys |
| tailscale.state | Fly Volume | Stays logged in |
| Market cache | Fly Volume | Faster restarts |

## Setup Checklist

### One-Time Setup

1. Install Fly CLI: `brew install flyctl`
2. Login: `fly auth login`
3. Create app: `fly launch --no-deploy --name arb-controller --region ams`
4. Create volume: `fly volumes create arb_data --region ams --size 1`
5. Set secrets:
   ```bash
   fly secrets set \
     TAILSCALE_AUTHKEY="tskey-auth-xxx" \
     KALSHI_API_KEY_ID="xxx" \
     KALSHI_KEY_B64="$(base64 < ~/.kalshi/private_key.pem)" \
     POLY_PRIVATE_KEY="0x..." \
     POLY_FUNDER="0x..."
   ```
6. Deploy: `fly deploy`
7. Add `FLY_API_TOKEN` to GitHub repo secrets (Settings → Secrets → Actions)

### Generate Fly Deploy Token

```bash
fly tokens create deploy -x 999999h
```

## Daily Usage

| Task | Command |
|------|---------|
| Start machine | `fly machine start` |
| SSH in | `fly ssh console` |
| Run bot | `controller` (inside SSH) |
| Stop machine | `fly machine stop` |
| Check status | `fly status` |
| View positions | `fly ssh console -C "cat /data/positions.json"` |
| Deploy new code | `git push` or `fly deploy` |

## Deployment Options

### Automatic (GitHub Actions)
Push to `main` triggers deploy via `.github/workflows/deploy.yml`

### Manual Remote Build
```bash
fly deploy
```
Uploads source, builds on Fly's servers.

### Manual Local Build
```bash
fly deploy --local-only
```
Builds on your machine, pushes image. Requires Docker.

## Future Enhancements

1. **Daemon mode**: Run bot in tmux/screen, attach via SSH
2. **Web UI**: Replace TUI with web interface for remote access
3. **Multi-region**: Run controller + trader both on Fly
4. **Monitoring**: Add health checks, alerting
