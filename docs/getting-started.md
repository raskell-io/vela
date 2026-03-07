# Getting Started

This guide walks you through setting up Vela on a server and deploying your first app.

## Prerequisites

- A Linux server (Hetzner dedicated, bare metal, or any VPS)
- SSH access with key-based auth
- Your app builds to a binary (Rust, Go, etc.) or a BEAM release (Elixir/Phoenix)

## 1. Install Vela on Your Server

SSH into your server and run the install script:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

This detects your platform (linux/amd64 or linux/arm64), downloads the latest release, and installs it to `/usr/local/bin/vela`.

To install a specific version or to a different directory:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash -s -- --version v0.3.0 --to /opt/bin
```

Create the server config:

```bash
mkdir -p /etc/vela
cat > /etc/vela/server.toml <<'EOF'
data_dir = "/var/vela"

[proxy]
http_port = 80
https_port = 443

[tls]
acme_email = "you@example.com"
EOF
```

Start the server as a systemd service (recommended):

```bash
# Generate and install the systemd unit file
vela setup

# Enable and start
sudo systemctl enable --now vela
```

Or run manually for testing:

```bash
RUST_LOG=info vela serve --config /etc/vela/server.toml
```

**Important:** The server config must be at `/etc/vela/server.toml`. All internal commands (`_deploy`, `_rollback`, `_secret`, `_logs`) use this as the default config path. If you use a non-default path, you'll need to configure the `--config` flag in your deploy commands.

The `vela serve` daemon must be running before you deploy. The deploy command communicates with the daemon via a Unix socket at `/var/vela/vela.sock`. If the daemon isn't running, deploys will fail with "failed to connect to vela daemon".

## 2. Install Vela on Your Laptop

Run the same install script — it detects macOS/Linux and amd64/arm64 automatically:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

Or install to a user-local directory (no sudo needed):

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash -s -- --to ~/.local/bin
```

Or build from source (requires Rust 1.93+):

```bash
cargo install --git https://github.com/raskell-io/vela
```

## 3. Initialize Your Project

In your app's repository:

```bash
cd my-app
vela init --name my-app --domain my-app.example.com
```

This creates a `Vela.toml`:

```toml
[app]
name = "my-app"
domain = "my-app.example.com"

[deploy]
server = "root@your-server.example.com"
type = "binary"
binary = "my-app"
health = "/health"

[env]
# DATABASE_PATH = "${data_dir}/my-app.db"

[resources]
# memory = "512M"
```

Edit `[deploy].server` to point at your server.

## 4. Deploy

Build your app and deploy:

```bash
# Rust app
cargo build --release
vela deploy ./target/release/my-app

# Elixir/Phoenix app
MIX_ENV=prod mix release
vela deploy ./_build/prod/rel/my_app
```

That's it. Vela uploads the artifact, starts it, runs a health check, and swaps traffic.

## 5. Verify

```bash
# Check status
vela status

# Tail logs
vela logs my-app -f

# Visit your app
curl https://my-app.example.com
```

For a complete pre-flight checklist (firewall, DNS, TLS staging, troubleshooting), see [Production Checklist](production-checklist.md).

## What Happens Under the Hood

1. `vela deploy` creates a tarball of your artifact
2. Uploads it to the server via `scp`
3. Server extracts it to `/var/vela/apps/my-app/releases/<timestamp>/`
4. Server starts the new binary on a random port
5. Server hits your health check endpoint until it returns 200
6. Proxy swaps traffic from old instance to new
7. Old instance drains connections and shuts down

Your app receives these environment variables:

| Variable | Value |
|----------|-------|
| `PORT` | The port to listen on |
| `VELA_PORT` | Same as `PORT` |
| `VELA_DATA_DIR` | Persistent data directory (survives deploys) |
| `VELA_APP_NAME` | Your app name |

Plus any variables you define in `[env]` in your `Vela.toml`.

## Updating Vela

Run the install script again to get the latest version:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

Or pin a specific version:

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash -s -- --version v0.3.0
```
