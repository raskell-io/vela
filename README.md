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
│  └── SQLite state                           │
│                                             │
│  Apps                                       │
│  ├── cyanea.bio      → :10001              │
│  └── archipelag.io   → :10002              │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│  Your laptop                                │
│                                             │
│  vela deploy  →  scp + ssh  →  server       │
└─────────────────────────────────────────────┘
```

- **One binary** — same `vela` runs on server and laptop
- **Embedded proxy** — Pingora (by Cloudflare) with automatic Let's Encrypt TLS
- **SSH is the control plane** — no tokens, no API keys, no custom auth
- **SQLite-aware** — persistent data directories survive deploys
- **Rust and Elixir** — deploy compiled binaries or BEAM releases

## Quick Start

### 1. Server Setup

```bash
# Install vela
curl -fsSL https://github.com/raskell-io/vela/releases/latest/download/vela-linux-amd64 \
  -o /usr/local/bin/vela && chmod +x /usr/local/bin/vela

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

## Status

Early development. The core deploy flow works. Building towards:

- [ ] Pingora proxy integration with auto-TLS
- [ ] `_deploy` internal command (server-side deploy receiver)
- [ ] systemd service generation
- [ ] Process isolation (namespaces, cgroups)
- [ ] Litestream integration for SQLite backups
- [ ] Multi-server deploys

## Building from Source

```bash
# Requires Rust 1.93+
cargo install --path .

# Or build a static Linux binary
cargo build --release --target x86_64-unknown-linux-musl
```

## License

MIT
