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
```

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
