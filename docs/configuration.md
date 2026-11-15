# Configuration

Vela has two config files: one for your project (client-side) and one for the server.

## Vela.toml (Project Manifest)

Lives in your project root. Read by `vela deploy` and other client commands.

```toml
[app]
name = "my-app"          # Required. App name (used in paths, logs, commands)
domain = "my-app.com"    # Required. Domain that routes to this app

[deploy]
server = "root@1.2.3.4"  # SSH target (user@host)
type = "binary"           # "binary" or "beam" (default: "binary")
binary = "my-app"         # Entrypoint name within the release directory
health = "/health"        # Health check path (GET, expects 200)
drain = 5                 # Seconds to drain old connections (default: 5)
strategy = "blue-green"   # "blue-green" or "sequential" (default: "blue-green")
pre_start = "bin/migrate" # Command to run before app starts (optional)
post_deploy = "bin/notify"# Command to run after traffic swap (optional)

[env]
DATABASE_PATH = "${data_dir}/my-app.db"
RUST_LOG = "info"

[resources]
memory = "512M"           # Memory limit (future)
cpu_weight = 100          # CPU weight, relative (future)
```

### App Types

**`binary`** — A compiled binary. Vela runs it directly. Use for Rust, Go, C, etc.

**`beam`** — An Elixir/Phoenix release (from `mix release`). Vela runs `bin/server start` (or whatever you set as `binary`). The release includes the BEAM runtime, so no Erlang/Elixir install is needed on the server.

### Deploy Strategies

**`blue-green`** (default) — Starts the new instance alongside the old one. After the health check passes, traffic swaps to the new instance and the old one drains. True zero downtime. Best for stateless apps.

**`sequential`** — Stops the old instance, then starts the new one. Sub-second blip. Use this for apps with SQLite databases to avoid write contention during the brief overlap window.

### Deploy Hooks

**`pre_start`** — Runs inside the release directory after extraction, before the app starts. If the command exits non-zero, the deploy aborts and the old instance stays running. Use for database migrations.

**`post_deploy`** — Runs after the health check passes and traffic switches to the new instance. Failures are logged but do not roll back the deploy. Use for cache warming, notifications, etc.

Both hooks run with the same environment variables as the app (secrets, manifest env, `PORT`, `VELA_DATA_DIR`).

### Manifest Env Persistence

The `[env]` section from your `Vela.toml` is persisted in `app.toml` on the server. When the Vela daemon restarts, it restores apps with their full environment (manifest env + secrets + Vela-injected vars like `PORT`).

### Environment Variable Substitution

Values in `[env]` support these variables:

| Variable | Expands to |
|----------|------------|
| `${data_dir}` | `/var/vela/apps/<name>/data` (persistent directory) |
| `${secret:KEY}` | The value of secret `KEY` (set via `vela secret set`) |

## server.toml (Server Config)

Lives at `/etc/vela/server.toml` on the server. Read by `vela serve`.

```toml
data_dir = "/var/vela"    # Where apps, releases, and state live (default: /var/vela)

[proxy]
http_port = 80            # HTTP listener port (default: 80)
https_port = 443          # HTTPS listener port (default: 443)

[tls]
acme_email = "ops@example.com"   # Email for Let's Encrypt registration
staging = false                   # Use LE staging environment (for testing)
# cert = "/etc/vela/tls/origin.pem"       # Static cert instead of ACME
# key = "/etc/vela/tls/origin-key.pem"
# client_ca = "/etc/vela/tls/client-ca.pem" # Require client certs signed by this CA (mTLS)
```

When `client_ca` is set, the HTTPS listener rejects any connection that doesn't present a client certificate signed by that CA. Use it for [Cloudflare Authenticated Origin Pulls](cloudflare.md#authenticated-origin-pulls-mtls) so only Cloudflare can reach your origin.

### Config Path

The server config **must** be at `/etc/vela/server.toml` for production use. All internal commands (`_deploy`, `_rollback`, `_secret`, `_logs`) default to this path. When the client runs `vela deploy`, it SSHs into the server and executes `vela _deploy <app>` — which reads from the default config path.

If you need a non-default path, pass `--config` to `vela serve` and ensure the same path is accessible to the internal commands.

### Daemon Requirement

The `vela serve` daemon must be running before you deploy. The daemon:
- Owns all app processes and their lifecycle
- Listens on a Unix socket at `<data_dir>/vela.sock` (default: `/var/vela/vela.sock`)
- Receives deploy/rollback commands from the `_deploy` and `_rollback` internal commands

Use `vela setup` to generate a systemd service file, then `sudo systemctl enable --now vela`.

### Defaults

If no server config file exists, Vela uses sensible defaults:
- Data directory: `/var/vela`
- HTTP on port 80, HTTPS on 443
- No TLS until `acme_email` is set

## Secrets

Secrets are stored on the server, not in your repo. Manage them with:

```bash
# Set a secret
vela secret set my-app SECRET_KEY_BASE=supersecretvalue

# List secrets (shows keys only, not values)
vela secret list my-app

# Remove a secret
vela secret remove my-app SECRET_KEY_BASE
```

Secrets are injected as environment variables when your app starts. Reference them in `Vela.toml` with `${secret:KEY}`.

Every client command (`deploy`, `status`, `logs`, `rollback`, `secret`, `backup`) runs `sudo vela …` on the server, because app state, secrets, logs and the daemon socket are root-owned. A non-root deploy user needs a sudoers entry that allows `vela`, for example `deploy ALL=(root) NOPASSWD: /usr/local/bin/vela`. Keys must be valid environment variable names. You can set secrets before an app's first deploy as long as the app name is lowercase (letters, digits, `-`, `_`, `.`).

## Status and Monitoring

Check the health of all running apps:

```bash
# Human-readable output
vela status
```

```
vela 0.4.0 — active, 2 app(s)

  cyanea          app.cyanea.bio          healthy (HTTP 200)   pid 38604   up 1d 21h
  coordinator     app.archipelag.io       healthy (HTTP 200)   pid 38148   up 1d 21h
```

```bash
# Machine-readable JSON (for monitoring scripts)
vela status --json
```

```json
[
  {
    "name": "cyanea",
    "domain": "app.cyanea.bio",
    "release": "20260306-145045",
    "strategy": "sequential",
    "pid": 38604,
    "port": 10001,
    "uptime_seconds": 162000,
    "health": "healthy"
  }
]
```

The `--json` output queries the running daemon via IPC for live process info. Each app's health endpoint is probed with a 3-second timeout. The `health` field is one of:

| Value | Meaning |
|-------|---------|
| `healthy` | Health endpoint returned HTTP 200 |
| `unhealthy` | Health endpoint failed or timed out |
| `unknown` | No health path configured for this app |

## Services (Service Dependencies)

Declare services your app depends on in `[services]`. Vela provisions them on first deploy and injects connection environment variables automatically.

### Postgres

```toml
[services.postgres]
version = "17"                # PostgreSQL version (default: "17")
databases = ["myapp_prod"]    # Databases to create
```

Vela installs PostgreSQL via apt (if not present), creates a user and database with a generated password, and injects:

| Variable | Value |
|----------|-------|
| `DATABASE_URL` | `postgres://<db>:<password>@localhost/<db>` |

Credentials are stored in `/var/vela/services/postgres/state.toml` and reused across deploys.

### NATS

```toml
[services.nats]
version = "2.10"    # NATS server version (default: "2.10")
jetstream = true    # Enable JetStream (default: false)
```

Vela downloads the NATS binary from GitHub releases, generates a config file, starts it as a supervised child process, and injects:

| Variable | Value |
|----------|-------|
| `NATS_URL` | `nats://localhost:4222` |

NATS listens on `127.0.0.1:4222` (client) and `127.0.0.1:8222` (monitoring/health).

### Any other service (Meilisearch, Redis, MinIO, …)

A `[services.<name>]` table with a `command` runs that command as a supervised sidecar. Vela does not install it: put the binary on the server first.

```toml
[services.meilisearch]
command = "/usr/local/bin/meilisearch"
args = ["--db-path", "${data_dir}", "--env", "production", "--http-addr", "127.0.0.1:7700"]
health = "http://127.0.0.1:7700/health"   # optional: must answer 2xx within 30s
restart = "always"                          # "always" (default), "on-failure" or "never"

[services.meilisearch.env]                  # environment for the service
MEILI_MASTER_KEY = "${secret:MEILI_MASTER_KEY}"

[services.meilisearch.export]               # environment injected into your app
MEILI_URL = "http://127.0.0.1:7700"
```

| Key | Meaning |
|-----|---------|
| `command` | Absolute path to the executable |
| `args` | Arguments, as a list |
| `env` | Environment for the service process |
| `export` | Environment variables your app receives (the app's own `[env]` wins on conflicts) |
| `health` | HTTP URL checked before your app starts; without it, the process must stay up for one second |
| `restart` | What to do when the process exits |

- `${data_dir}` expands to `/var/vela/services/<name>/data`, which persists across deploys and is the service's working directory.
- `${secret:KEY}` expands to the app's secret `KEY` (`vela secret set <app> KEY=VALUE`), so keys stay out of `Vela.toml`.
- The service starts before your app on every deploy. If its spec changed, it is restarted first; if it fails to start or fails its health check, the deploy fails and the running app stays.
- Vela restarts crashed services according to `restart`, restores them when the daemon starts, and stops them (SIGTERM, then SIGKILL after 10s) when it shuts down.
- `vela status` lists services with their pid, uptime and restart count. Logs go to `/var/vela/services/<name>/service.log`.
- Names use lowercase letters, digits, `-` and `_`. `postgres` and `nats` are reserved for the built-in providers above.
- Removing a service from `Vela.toml` does not stop it yet; stop it with `systemctl restart vela` after deleting `/var/vela/services/<name>/service.toml`.

## Build (Remote Builds)

Build your app on the server instead of locally. Useful for Elixir releases or when cross-compilation is impractical.

```toml
[build]
remote = true                  # Enable remote build (default: false)
command = "mix release"        # Build command to run on the server

[build.env]
MIX_ENV = "prod"               # Environment variables for the build
```

When `remote = true`, `vela deploy` uploads your source via `git archive` (no artifact argument needed), runs the build command on the server, and activates the result through the normal deploy flow.

**Note:** Your project must be a git repository. Only committed files are uploaded (via `git archive HEAD`).

## Backup (Server Config)

Configure scheduled backups in `server.toml`. Vela backs up app data, secrets, and Postgres databases automatically.

```toml
[backup]
schedule = "daily"                    # "hourly", "daily", or interval like "12h"
retain = 7                            # Number of backups to keep
destination = "/var/backups/vela"     # Local path or "s3://bucket/prefix"

[backup.include]
app_data = true     # Back up app data directories (default: true)
secrets = true      # Back up secrets.env files (default: true)
postgres = true     # Back up Postgres databases via pg_dump (default: true)
```

### Destinations

**Local directory** — Backups are copied to the specified path. Old backups are deleted when count exceeds `retain`.

**S3-compatible storage** — Set `destination = "s3://bucket/prefix"`. Requires the `aws` CLI to be installed and configured on the server.

### What Gets Backed Up

- **App data**: Persistent data directories (`/var/vela/apps/<app>/data/`). SQLite WAL files are checkpointed before copy for consistency.
- **Secrets**: `secrets.env` and `app.toml` config for each app.
- **Postgres**: `pg_dump` of each provisioned database (gzip compressed).

### Manual Backup

Trigger a backup from your laptop:

```bash
vela backup
```

This SSHs into the server and runs the backup immediately using the `[backup]` config from `server.toml`.
