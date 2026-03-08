# Architecture

How Vela works internally.

## Single Binary, Two Modes

Vela ships as one binary. The same binary runs on your server and your laptop.

```
vela
├── serve           → Server mode (Linux only)
│   ├── Reverse proxy (hyper)
│   ├── Process manager
│   ├── Service manager (Postgres, NATS)
│   ├── Backup scheduler
│   ├── IPC daemon (Unix socket)
│   └── Filesystem state
│
├── deploy          → Client mode (any platform)
│   ├── Read Vela.toml
│   ├── Create tarball (or git archive for remote builds)
│   ├── Upload via scp
│   └── Activate via ssh
│
├── init / status / logs / rollback / secret / backup
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

Vela embeds a hyper-based reverse proxy with TLS termination via tokio-rustls.

```
Internet → :443 (TLS) → hyper → route by Host header → app on :10xxx
         → :80  (redirect to HTTPS, except ACME challenges)
```

The route table maps domains to upstream ports:

```
cyanea.bio    → 127.0.0.1:10001
archipelag.io → 127.0.0.1:10002
```

When a deploy swaps, the route table updates atomically. Old connections drain for `drain_seconds` before the previous instance is stopped.

### TLS

Two modes:

- **ACME (Let's Encrypt)** — Set `acme_email` in server.toml. Vela provisions certs on first request and renews them automatically when they're within 30 days of expiry. HTTP-01 challenge validation on port 80.
- **Static certs** — Set `cert` and `key` paths in server.toml. Use with Cloudflare Origin Certificates or any custom cert.

HTTP requests are automatically redirected to HTTPS (301) when TLS is configured, except for `/.well-known/acme-challenge/` paths needed for ACME validation.

## IPC Architecture

The `vela serve` daemon owns all app processes. Client-initiated operations (deploy, rollback) communicate with the daemon via a Unix socket at `/var/vela/vela.sock`.

```
vela deploy (laptop)
  → ssh root@server vela _deploy <app>
    → connects to /var/vela/vela.sock
      → daemon starts new process, health checks, swaps proxy

vela status --json (laptop)
  → ssh root@server vela apps --json
    → connects to /var/vela/vela.sock
      → daemon reports live PID, port, uptime, health for each app
```

This ensures the daemon is always the parent of all app processes and can supervise them.

## Process Manager

Each app runs as a child process of the Vela daemon. The process manager:

1. **Allocates a port** from the range 10000-20000
2. **Starts the process** with `PORT`, `VELA_DATA_DIR`, and user-defined env vars
3. **Monitors health** via HTTP health checks
4. **Manages the swap** (blue-green or sequential)
5. **Handles signals** for graceful shutdown (SIGTERM → wait `drain_seconds` → SIGKILL)
6. **Supervises processes** — automatically restarts crashed apps

### Process Supervision

When a deployed app process exits unexpectedly, the daemon restarts it automatically using the stored launch configuration (port, env vars, release directory). Intentional stops during deploys or rollbacks do not trigger auto-restart.

## State

Server state is entirely filesystem-backed. No database.

```
/var/vela/
├── vela.sock                    # IPC Unix socket
├── tls/                         # ACME certificates
│   ├── cyanea.bio.pem
│   └── cyanea.bio-key.pem
├── logs/                        # App stdout/stderr logs
└── apps/
    └── my-app/
        ├── app.toml             # App config (name, domain, type, strategy, env)
        ├── secrets.env          # KEY=VALUE, mode 0600
        ├── data/                # Persistent (never touched by deploys)
        │   └── my-app.db        # SQLite databases go here
        ├── releases/
        │   ├── 20260305-001/    # Old release (kept for rollback)
        │   └── 20260305-002/    # Current release
        └── current -> releases/20260305-002
```

Key invariants:
- **Deploy never touches `/data`**. Databases, uploads, and persistent state survive across deploys.
- **Manifest `[env]` vars are persisted** in `app.toml` and restored on daemon restart.
- **Secrets stay separate** from config in `secrets.env`, mode 0600.

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

## Deploy Hooks

Two hooks run at specific points during deployment:

- **`pre_start`** — Runs after extraction, before the app starts. If it fails (non-zero exit), the deploy aborts and the old instance stays. Use for database migrations.
- **`post_deploy`** — Runs after traffic switches to the new instance. Failures are logged but don't roll back. Use for cache warming, notifications, etc.

Both hooks run with the same environment variables as the app and inherit the release directory as their working directory.

## Service Dependencies

Vela manages external services that your apps depend on. Declare them in `[services]` in your `Vela.toml` and Vela handles provisioning, supervision, and environment variable injection.

### Supported Services

**Postgres** — Installed via `apt`, managed by systemd. Vela creates databases and users with generated passwords, verifies readiness via `pg_isready`, and injects `DATABASE_URL` into your app.

**NATS** — Downloaded as a binary from GitHub releases. Runs as a daemon child process (supervised alongside your apps). Supports JetStream. Injects `NATS_URL` into your app.

### How It Works

```
vela deploy (with [services] in Vela.toml)
  → IPC to daemon with service config
    → ServiceManager.ensure_service()
      → Install/download if needed
      → Provision databases/config
      → Start process (NATS) or verify running (Postgres)
      → Return env vars (DATABASE_URL, NATS_URL)
    → Merge env vars into app environment
    → Start app with injected env
```

Service state is persisted to `/var/vela/services/<type>/state.toml`. On daemon restart, services are restored automatically — Postgres via systemd, NATS by relaunching the child process.

### Service Supervision

The daemon supervises NATS alongside your apps. If the NATS process exits unexpectedly, it is restarted automatically. Postgres is managed by systemd and checked for readiness on restore.

### Filesystem Layout

```
/var/vela/services/
├── postgres/
│   └── state.toml          # Tracks provisioned databases and credentials
└── nats/
    ├── nats-server          # Downloaded binary
    ├── nats.conf            # Generated config (listen, JetStream, etc.)
    ├── nats.log             # NATS stdout/stderr
    ├── data/                # JetStream storage
    └── state.toml           # Port, provisioned flag
```

## Scheduled Backups

Vela can automatically back up app data, secrets, and Postgres databases on a schedule. Configure `[backup]` in `server.toml`.

Backups include:
- **App data** — SQLite databases and persistent files (with WAL checkpoint before copy)
- **Secrets** — `secrets.env` and `app.toml` config files
- **Postgres** — `pg_dump` of all provisioned databases (gzip compressed)

Destinations: local directory or S3-compatible storage (via `aws` CLI). Old backups are pruned according to the `retain` count.

## Remote Builds

For apps that need to be built on the server (e.g., Elixir releases), Vela supports remote builds. Set `[build] remote = true` in your `Vela.toml`.

```
vela deploy (with [build] remote = true)
  → git archive → upload source via scp
  → ssh: extract source, run build command
  → ssh: tar build output, activate via normal deploy flow
```

Source is uploaded via `git archive` (excludes node_modules, target, etc.). The build runs on the server with configurable environment variables.
