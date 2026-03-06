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
│  Vela                                       │
│  ├── Reverse proxy (:80/:443, auto-TLS)     │
│  ├── Process manager (start, health, swap)  │
│  └── State manager                          │
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
- **Embedded proxy** — hyper-based reverse proxy with TLS support
- **SSH is the control plane** — no tokens, no API keys, no custom auth
- **SQLite-aware** — persistent data directories survive deploys
- **Rust and Elixir** — deploy compiled binaries or BEAM releases

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

Or specify a version and install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash -s -- --version v0.1.0 --to ~/.local/bin
```

Binaries are available for Linux (amd64, arm64) and macOS (amd64, arm64).

## Quick Start

### 1. Server Setup

```bash
# Install vela on your server
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash

# Start
vela serve
```

### 2. Project Setup

```bash
cd my-app
vela init --name my-app --domain my-app.example.com
# Edit Vela.toml → set deploy.server
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

[resources]
memory = "512M"
```

## Commands

| Command | Description |
|---------|-------------|
| `vela serve` | Start the server (Linux) |
| `vela init` | Generate a Vela.toml |
| `vela deploy <artifact>` | Deploy an app |
| `vela status` | Show running apps |
| `vela logs <app> [-f]` | Tail app logs |
| `vela rollback <app>` | Roll back to previous release |
| `vela secret set <app> KEY=VALUE` | Set a secret |
| `vela apps` | List apps (server-side) |

## Deploy Strategies

**Blue-green** (default) — Two instances run briefly during the swap. True zero downtime.

**Sequential** — Old stops, new starts. Sub-second blip. Use for SQLite apps to avoid write contention.

## Your App Needs To

1. Listen on `$PORT` (Vela assigns it)
2. Expose a health endpoint (return 200 when ready)
3. Handle `SIGTERM` for graceful shutdown
4. Use `$VELA_DATA_DIR` for persistent files (databases, uploads)

## Documentation

- [Getting Started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Deploy Lifecycle](docs/deploy-lifecycle.md)
- [Architecture](docs/architecture.md)
- [Elixir/Phoenix Guide](docs/elixir-phoenix.md)
- [Cloudflare Integration](docs/cloudflare.md)

## Status

Core functionality is built and working:

- [x] Single binary (server + client)
- [x] SSH-based deploy pipeline (upload → extract → health check → swap)
- [x] Hyper-based reverse proxy with TLS (Cloudflare Origin Certs)
- [x] Blue-green and sequential deploy strategies
- [x] Process manager with port allocation and graceful shutdown
- [x] Filesystem-backed state (no database dependency)
- [x] Secret management (per-app, mode 0600)
- [x] Rollback to previous release
- [x] App restore on server restart
- [x] Elixir/Phoenix BEAM release support
- [x] CI/CD pipeline with multi-platform release builds
- [x] Install script

- [x] Let's Encrypt auto-TLS (ACME HTTP-01 with SNI-based cert resolution)
- [x] systemd service generation (`vela setup`)
- [x] Log file capture and streaming (`vela logs -f`)
- [x] Release sandbox (read-only release directories)

Coming next:

- [ ] Process isolation (namespaces, cgroups v2)
- [ ] Litestream integration for SQLite backups
- [ ] Certificate auto-renewal
- [ ] Multi-server deploys

## Building from Source

```bash
# Requires Rust 1.93+
cargo install --path .
```

## Release Process

Tag a version to trigger the release pipeline:

```bash
git tag v0.1.0
git push --tags
```

GitHub Actions builds binaries for all platforms and attaches them to the release with checksums.

## License

MIT
