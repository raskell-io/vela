# Architecture

How Vela works internally.

## Single Binary, Two Modes

Vela ships as one binary. The same binary runs on your server and your laptop.

```
vela
├── serve           → Server mode (Linux only)
│   ├── Reverse proxy (Pingora)
│   ├── Process manager
│   ├── Deploy receiver
│   └── SQLite state
│
├── deploy          → Client mode (any platform)
│   ├── Read Vela.toml
│   ├── Create tarball
│   ├── Upload via scp
│   └── Activate via ssh
│
├── init / status / logs / rollback / secret
│   └── Client commands (SSH into server)
│
└── apps            → Server-side management (Linux only)
```

Server code is `#[cfg(target_os = "linux")]`. The client commands work on macOS and Linux.

## Why No Containers

Docker solves a real problem (reproducible environments), but it adds:
- A daemon process
- Image layers and registries
- Overlay filesystems
- Network namespaces with iptables rules
- A whole build system (Dockerfiles)

For deploying compiled binaries and BEAM releases to your own server, none of this is needed. Your binary runs on Linux. Ship the binary.

Vela uses Linux process isolation where it matters:
- Separate Unix user per app (v1)
- systemd sandboxing: PrivateTmp, ProtectSystem, ReadOnlyDirectories (v1)
- PID/mount/network namespaces via `nix` crate (future)
- cgroups v2 for memory/CPU limits (future)

## Reverse Proxy

Vela embeds [Pingora](https://github.com/cloudflare/pingora), Cloudflare's Rust proxy framework.

```
Internet → :443 (TLS) → Pingora → route by Host header → app on :10xxx
         → :80  (redirect to HTTPS)
```

The route table maps domains to upstream ports:

```
cyanea.bio    → 127.0.0.1:10001
archipelag.io → 127.0.0.1:10002
```

When a deploy swaps, the route table updates atomically. Pingora handles connection draining.

TLS certificates are provisioned automatically via Let's Encrypt (ACME protocol).

## Process Manager

Each app runs as a child process of the Vela daemon. The process manager:

1. **Allocates a port** from the range 10000-20000
2. **Starts the process** with `PORT`, `VELA_DATA_DIR`, and user-defined env vars
3. **Monitors health** via HTTP health checks
4. **Manages the swap** (blue-green or sequential)
5. **Handles signals** for graceful shutdown

Processes are tracked in memory with their PID and port. State (which release is active) is persisted in SQLite.

## State

Server state lives in `/var/vela/vela.db` (SQLite):

```sql
apps       — registered apps (name, domain, type, health path, strategy)
releases   — deploy history (app, release_id, status, timestamps)
secrets    — encrypted env vars per app
```

SQLite is the right choice here: single-writer, crash-safe, zero-config, and the state is small.

## SSH as Control Plane

There is no custom API server, no authentication system, no tokens. Client commands work by:

1. SSH into the server (using your existing SSH keys)
2. Run `vela` subcommands on the remote side
3. Display the output locally

This means:
- Security = SSH key management (which you already do)
- No ports to open (beyond 22, 80, 443)
- No TLS certs for an admin API
- Works through firewalls and bastion hosts
- `scp` for file transfer (artifact upload)

## Directory Layout

```
/var/vela/                        # Server root
├── vela.db                       # SQLite state
├── secrets/                      # Per-app secret env files
│   ├── my-app.env
│   └── other-app.env
└── apps/
    └── my-app/
        ├── app.toml              # App config (from Vela.toml)
        ├── data/                 # Persistent data (survives deploys)
        │   └── my-app.db         # SQLite databases go here
        ├── releases/
        │   ├── 20260305-001/     # Old release (kept for rollback)
        │   └── 20260305-002/     # Current release
        └── current -> releases/20260305-002
```

Key invariant: **deploy never touches `/data`**. Your database files, uploaded assets, and any other persistent state live in the data directory and survive across deploys.

## Future: Multi-Server

The current design is single-server. Future versions may support:
- Multiple servers in a `Vela.toml` (deploy to all)
- Health-based routing across servers
- SQLite replication via Litestream for backups

But single-server covers a surprising range of use cases, and that's the focus for now.
