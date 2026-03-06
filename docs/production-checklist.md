# Production Checklist

Everything you need before deploying to a real server.

## Server Prerequisites

- [ ] Linux server (Hetzner dedicated, VPS, or any bare metal)
- [ ] SSH key-based access (`ssh root@your-server` works without a password)
- [ ] Ports 80 and 443 open in your firewall
- [ ] DNS A record pointing your domain(s) at the server IP

### Firewall

If you're using `ufw`:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
```

If you're using Hetzner's firewall, add inbound rules for TCP 80 and 443 in the Cloud Console.

### DNS

ACME HTTP-01 validation requires your domain to resolve to the server **before** the first deploy. Set your A record and verify:

```bash
dig +short your-app.example.com
# Should return your server's IP
```

## Server Setup

### 1. Install Vela

```bash
# On your server
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

Or copy the binary from a GitHub release:

```bash
scp vela-linux-amd64 root@your-server:/usr/local/bin/vela
ssh root@your-server chmod +x /usr/local/bin/vela
```

### 2. Create Server Config

```bash
mkdir -p /etc/vela
cat > /etc/vela/server.toml <<'EOF'
data_dir = "/var/vela"

[proxy]
http_port = 80
https_port = 443

[tls]
acme_email = "you@example.com"
staging = true  # Start with staging, switch to false once verified
EOF
```

**Start with `staging = true`**. Let's Encrypt production has strict rate limits (5 duplicate certificates per week). Use staging to verify everything works, then flip to `false` for a real certificate.

### 3. Install and Start the Daemon

```bash
vela setup
sudo systemctl enable --now vela
sudo systemctl status vela    # Verify it's running
journalctl -u vela -f         # Watch logs
```

## App Requirements

### Your App Must Listen on `$PORT`

Vela assigns a random port (10000-20000) to each app instance. Your app **must** read the `PORT` environment variable and bind to it. Do not hardcode a port.

Rust example:

```rust
let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
let addr = format!("0.0.0.0:{port}");
```

Elixir/Phoenix example (in `config/runtime.exs`):

```elixir
port = String.to_integer(System.get_env("PORT") || "4000")
config :my_app, MyAppWeb.Endpoint, http: [port: port]
```

### Health Check Endpoint

If you set `health = "/health"` in your `Vela.toml`, your app must:

- Respond to `GET /health` with HTTP 200
- Do so within ~30 seconds of startup
- Bind and start accepting connections quickly

If your app takes longer than 30 seconds to boot, the deploy will fail and the old instance stays active (this is by design — no broken deploys reach production).

If you don't set a health path, Vela waits 2 seconds after startup and assumes the app is ready.

### Bind to All Interfaces

Your app should bind to `0.0.0.0`, not `127.0.0.1`. The proxy forwards traffic to `127.0.0.1:<port>`, so binding to localhost works — but some frameworks default to IPv6 `::1` which won't match.

## Client Setup (Your Laptop)

### 1. Install Vela

```bash
curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
```

### 2. Create Vela.toml

```bash
cd your-project
vela init --name your-app --domain your-app.example.com
```

Edit the generated `Vela.toml`:

```toml
[app]
name = "your-app"
domain = "your-app.example.com"

[deploy]
server = "root@your-server-ip"
type = "binary"
binary = "your-app"
health = "/health"
```

### 3. Deploy

```bash
cargo build --release
vela deploy ./target/release/your-app
```

## First Deploy Checklist

- [ ] DNS resolves to server IP
- [ ] Ports 80/443 open
- [ ] `vela serve` daemon running (`systemctl status vela`)
- [ ] `staging = true` in server.toml (for first test)
- [ ] App listens on `$PORT`
- [ ] App responds 200 on health check path (if configured)
- [ ] `Vela.toml` has correct `server`, `domain`, and `binary`

## Switching from Staging to Production TLS

Once your first deploy works with staging certificates (browser will show a security warning — that's expected):

```bash
# On your server
sed -i 's/staging = true/staging = false/' /etc/vela/server.toml

# Delete the staging cert so Vela provisions a real one
rm -f /var/vela/tls/your-app.example.com.*

# Restart the daemon
sudo systemctl restart vela
```

The next request to your domain will trigger real certificate provisioning. This takes 10-30 seconds.

## Post-Deploy Operations

```bash
# Check app status
vela status

# Tail logs
vela logs your-app -f

# View stderr
vela logs your-app --stderr

# Set a secret
vela secret set your-app DATABASE_URL=postgres://...

# Roll back to previous release
vela rollback your-app
```

## Troubleshooting

### "failed to connect to vela daemon"

The `vela serve` daemon isn't running. Check:

```bash
sudo systemctl status vela
journalctl -u vela --no-pager -n 50
```

### Deploy succeeds but app unreachable

1. Check the proxy is listening: `ss -tlnp | grep ':80\|:443'`
2. Check the route table: `journalctl -u vela | grep "deploy activated"`
3. Check your DNS resolves to the right IP
4. Check the app is actually running: `journalctl -u vela | grep "started app"`

### Health check fails

Your app isn't responding on the assigned port within 30 seconds. Check:

1. App logs: `vela logs your-app --stderr`
2. Is the app reading `$PORT`?
3. Is the app binding to `0.0.0.0` (not just `127.0.0.1` or `::1`)?
4. Does the health endpoint return 200 (not 301, 404, etc.)?

### ACME certificate not provisioning

1. Is `acme_email` set in server.toml?
2. Does DNS resolve to this server? (`dig +short your-domain`)
3. Is port 80 reachable from the internet? (ACME HTTP-01 needs this)
4. Check daemon logs: `journalctl -u vela | grep -i acme`
5. Are you hitting rate limits? Use `staging = true` to test first.
