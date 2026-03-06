# Vela

No-downtime app deployment on bare metal. One binary. No containers. No bullshit.

---

Buy a server. Install Vela. Deploy your apps from your laptop over SSH. That's it.

No Docker. No Kubernetes. No YAML-driven orbital complexity. No $500/month for an app that nobody uses yet.

## What It Does

```bash
# On your server
vela serve

# From your laptop
vela deploy ./target/release/my-app
```

Vela uploads your binary (or BEAM release), starts it on a fresh port, runs a health check, swaps the proxy, and drains the old instance. Zero downtime. Under a minute.

## How It Works

```
┌─────────────────────────────────────────────┐
│  Your server                                │
│                                             │
│  Vela daemon                                │
│  ├── Reverse proxy (:80/:443, auto-TLS)     │
│  ├── Process manager (start, health, swap)  │
│  └── IPC socket (/var/vela/vela.sock)       │
│                                             │
│  Apps                                       │
│  ├── next.ai         → :10001               │
│  └── giga.app        → :10002               │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│  Your laptop                                │
│                                             │
│  vela deploy  →  scp + ssh  →  server       │
└─────────────────────────────────────────────┘
```

- **One binary** — same `vela` runs on server and laptop
- **Embedded proxy** — hyper-based reverse proxy with auto-TLS via Let's Encrypt
- **SSH is the control plane** — no tokens, no API keys, no custom auth
- **SQLite-aware** — persistent data directories survive deploys; sequential strategy avoids write contention
- **Rust and Elixir** — deploy compiled binaries or BEAM releases

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

Binaries are available for Linux (amd64, arm64) and macOS (amd64, arm64).

## Quick Start

### 1. Server Setup

```bash
# Install vela on your server
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash

# Create config
mkdir -p /etc/vela
cat > /etc/vela/server.toml <<'EOF'
data_dir = "/var/vela"

[proxy]
http_port = 80
https_port = 443

[tls]
acme_email = "you@example.com"
staging = true  # Use staging first, switch to false once verified
EOF

# Install systemd service and start
vela setup
sudo systemctl enable --now vela
```

### 2. Project Setup

```bash
cd my-app
vela init --name my-app --domain my-app.example.com
# Edit Vela.toml → set deploy.server = "root@your-server-ip"
```

### 3. Deploy

```bash
cargo build --release
vela deploy ./target/release/my-app
```

### Elixir/Phoenix

```bash
MIX_ENV=prod mix release
vela deploy ./_build/prod/rel/my_app
```

See [docs/elixir-phoenix.md](docs/elixir-phoenix.md) for the full guide.

## Vela.toml

```toml
[app]
name = "my-app"
domain = "my-app.example.com"

[deploy]
server = "root@your-server.example.com"
type = "binary"              # or "beam" for Elixir
binary = "my-app"
health = "/health"
strategy = "blue-green"      # or "sequential" for SQLite apps

[env]
DATABASE_PATH = "${data_dir}/my-app.db"
SECRET_KEY_BASE = "${secret:SECRET_KEY_BASE}"
```

## Commands

| Command | Description |
|---------|-------------|
| `vela serve` | Start the server daemon (Linux) |
| `vela setup` | Generate and install systemd service |
| `vela init` | Generate a Vela.toml |
| `vela deploy <artifact>` | Deploy an app |
| `vela status` | Show running apps |
| `vela logs <app> [-f]` | Tail app logs |
| `vela rollback [<app>]` | Roll back to previous release |
| `vela secret set <app> KEY=VALUE` | Set a secret |
| `vela secret list <app>` | List secret keys |
| `vela secret remove <app> KEY` | Remove a secret |
| `vela apps` | List apps (server-side) |

## Deploy Strategies

**Blue-green** (default) — New instance starts alongside old. After health check passes, traffic swaps and old instance drains. True zero downtime.

**Sequential** — Old instance stops, new instance starts. Sub-second blip. Use for SQLite apps to avoid write contention during the overlap window.

## Your App Needs To

1. **Listen on `$PORT`** — Vela assigns a random port (10000-20000). Your app must read the `PORT` env var and bind to it.
2. **Respond on your health path** — If you set `health = "/health"`, return HTTP 200 within 30 seconds of startup.
3. **Handle `SIGTERM`** — Vela sends SIGTERM for graceful shutdown, then SIGKILL after a timeout.
4. **Use `$VELA_DATA_DIR` for persistent files** — databases, uploads, anything that survives deploys.

## Documentation

- [Getting Started](docs/getting-started.md) — first install and deploy walkthrough
- [Production Checklist](docs/production-checklist.md) — pre-flight checks, troubleshooting, ACME staging workflow
- [Configuration](docs/configuration.md) — Vela.toml and server.toml reference
- [Deploy Lifecycle](docs/deploy-lifecycle.md) — what happens during a deploy
- [Architecture](docs/architecture.md) — system design and internals
- [Elixir/Phoenix Guide](docs/elixir-phoenix.md) — deploying BEAM releases
- [Cloudflare Integration](docs/cloudflare.md) — using Cloudflare with Vela

## Status

All core functionality is built, tested, and working:

- [x] Single binary (server + client)
- [x] SSH-based deploy pipeline (tarball upload → extract → health check → swap)
- [x] Reverse proxy with domain-based routing (hyper)
- [x] Auto-TLS via Let's Encrypt (ACME HTTP-01 with SNI-based cert resolution)
- [x] Static TLS support (Cloudflare Origin Certs, custom certs)
- [x] Blue-green and sequential deploy strategies
- [x] Process manager with port allocation and graceful shutdown (SIGTERM → SIGKILL)
- [x] IPC daemon architecture (Unix socket for deploy coordination)
- [x] Filesystem-backed state (no database dependency)
- [x] Secret management (per-app, file-backed, mode 0600)
- [x] Environment variable substitution (`${data_dir}`, `${secret:KEY}`)
- [x] Rollback to previous release
- [x] App restore on daemon restart
- [x] Log capture and streaming (`vela logs -f`)
- [x] Release sandbox (read-only release directories)
- [x] Elixir/Phoenix BEAM release support
- [x] systemd service generation with hardening (`vela setup`)
- [x] CI/CD pipeline with multi-platform release builds
- [x] Install script (auto-detects platform)

Coming next:

- [ ] Process isolation (Linux namespaces, cgroups v2)
- [ ] Resource limits (memory, CPU weight)
- [ ] Certificate auto-renewal
- [ ] Litestream integration for SQLite backups
- [ ] Multi-server deploys

## Building from Source

```bash
# Requires Rust 1.93+
cargo install --path .
```

## License

MIT
