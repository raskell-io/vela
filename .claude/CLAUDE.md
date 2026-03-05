# Vela

> **No-downtime app deployment on bare metal. One binary. No containers. No bullshit.**

Vela is a deployment tool for people who own their servers. You install `vela` on a bare metal machine, and from your laptop you deploy apps over SSH with zero downtime. No Docker, no Kubernetes, no YAML-driven orbital complexity.

## Philosophy

1. **One binary, two modes** — `vela serve` on the server, `vela deploy` from your laptop. Same binary.
2. **No containers** — Apps run as isolated processes with Linux namespaces, cgroups v2, and systemd sandboxing. Ship binaries or BEAM releases, not images.
3. **Zero downtime by default** — Blue-green deploys with health checks. Traffic switches only after the new instance is healthy.
4. **SQLite-aware** — Persistent data directories survive deploys. Sequential swap for write-heavy SQLite apps to avoid contention. No SQLite for Vela's own state — pure filesystem.
5. **Automatic TLS** — Embedded reverse proxy with Let's Encrypt. No nginx, no Caddy, no manual cert management.
6. **SSH is the control plane** — No custom auth, no tokens, no API keys. If you can SSH in, you can deploy.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  vela serve (on bare metal server)                  │
│                                                     │
│  ┌──────────────┐  ┌────────────────────────────┐   │
│  │  Proxy       │  │  Process Manager            │   │
│  │  (Pingora)   │  │                             │   │
│  │              │  │  app1 (Rust binary)         │   │
│  │  :80 / :443  │──│  app2 (BEAM release)        │   │
│  │  auto-TLS    │  │  app3 (any binary)          │   │
│  │              │  │                             │   │
│  └──────────────┘  └────────────────────────────┘   │
│                                                     │
│  ┌──────────────┐  ┌────────────────────────────┐   │
│  │  Deploy      │  │  State (filesystem)         │   │
│  │  Receiver    │  │  /var/vela/apps/            │   │
│  │  (SSH)       │  │  app.toml + secrets.env     │   │
│  └──────────────┘  └────────────────────────────┘   │
└─────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────┐
│  vela deploy (from your laptop)                     │
│                                                     │
│  Reads Vela.toml → uploads artifact via SSH →       │
│  tells server to activate                           │
└─────────────────────────────────────────────────────┘
```

## Single Binary Design

Vela is one Rust binary with subcommands:

### Server-side (run on the bare metal machine)
- `vela serve` — Start the daemon (proxy + process manager)
- `vela apps` — List running apps
- `vela app <name> status|logs|rollback` — Manage a specific app

### Client-side (run from your laptop)
- `vela deploy` — Deploy an app (reads Vela.toml, uploads artifact, activates)
- `vela status` — Show status of apps on a remote server
- `vela logs <app>` — Tail logs from a remote app
- `vela secret set <app> KEY=VALUE` — Set a secret on the server
- `vela rollback <app>` — Roll back to the previous release
- `vela init` — Generate a Vela.toml for a project

Client commands SSH into the server and run `vela` subcommands there.

## App Model

```
/var/vela/
├── apps/
│   └── cyanea/
│       ├── app.toml           # App config (name, domain, type, strategy)
│       ├── secrets.env        # KEY=VALUE, mode 0600
│       ├── data/              # Persistent across deploys
│       │   └── cyanea.db      # SQLite databases live here
│       ├── releases/
│       │   ├── 20260305-001/  # Each deploy is a directory
│       │   │   └── cyanea     # Binary, BEAM release, or tarball contents
│       │   └── 20260305-002/
│       │       └── cyanea
│       └── current -> releases/20260305-002
```

## Deploy Manifest (Vela.toml)

```toml
[app]
name = "cyanea"
domain = "cyanea.bio"

[deploy]
server = "root@my-server.example.com"
type = "binary"            # or "beam" for Elixir releases
binary = "cyanea-server"   # entrypoint within the release dir
health = "/health"         # GET this path, expect 200
drain = 5                  # seconds to drain old connections

[env]
DATABASE_PATH = "${data_dir}/cyanea.db"

[resources]
memory = "512M"
```

## Key Directories

| Path | Purpose |
|------|---------|
| `src/main.rs` | CLI entry point, clap subcommands |
| `src/server/` | Server daemon: proxy, process manager, deploy receiver |
| `src/server/proxy.rs` | Pingora-based reverse proxy with auto-TLS |
| `src/server/process.rs` | App lifecycle: start, stop, health check, swap |
| `src/server/deploy.rs` | Receive and activate deployments |
| `src/server/state.rs` | Filesystem-backed state (apps, releases, secrets) |
| `src/client/` | Client commands: deploy, status, logs |
| `src/client/ssh.rs` | SSH transport (upload artifacts, run remote commands) |
| `src/config.rs` | Vela.toml parsing and server config |
| `src/health.rs` | Health check logic |

## Rules

| File | Purpose |
|------|---------|
| [rust-standards.md](rules/rust-standards.md) | Rust coding standards |
| [architecture.md](rules/architecture.md) | Architecture decisions and constraints |

## Quick Reference

### Common Commands

```bash
# Development
cargo build
cargo test
cargo clippy -- -D warnings

# Run locally (server mode)
cargo run -- serve --config vela.toml

# Run locally (deploy)
cargo run -- deploy --manifest Vela.toml ./target/release/myapp
```

### Dependencies

- `pingora` — Reverse proxy (Cloudflare)
- `tokio` — Async runtime
- `clap` — CLI parsing
- `toml` — Config and state serialization
- `tracing` — Structured logging
- `thiserror` / `anyhow` — Error handling
- `serde` / `toml` — Config parsing
- `nix` — Linux namespace/cgroup syscalls (server-only)

### Deploy Flow

1. `vela deploy` reads `Vela.toml`
2. Compresses artifact directory into a tarball
3. Uploads tarball to server via SSH (scp/rsync)
4. Server extracts to `/var/vela/apps/<name>/releases/<timestamp>/`
5. Server starts new instance on a random port
6. Server runs health check (`GET /health`)
7. On success: proxy swaps traffic to new instance, old instance drains
8. On failure: new instance killed, old instance stays, deploy fails loudly

### Zero Downtime Strategy

- **Stateless apps (Rust binaries)**: True blue-green. Two instances run briefly during swap.
- **SQLite apps (Phoenix/Elixir)**: Sequential swap. Old stops, new starts. Sub-second blip. WAL mode recommended.
- Configurable per-app via `[deploy] strategy = "blue-green" | "sequential"`
