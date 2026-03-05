# Cloudflare + Vela on Hetzner

How to put Cloudflare in front of your Vela-managed Hetzner server.

## Architecture

```
User → Cloudflare (:443, CF edge cert) → Your Hetzner server (:443, origin cert) → Vela → app
```

Cloudflare handles public-facing TLS, DDoS protection, and caching. Vela handles routing, deploys, and process management. The connection between Cloudflare and your server is encrypted with a Cloudflare Origin Certificate.

## Setup

### 1. DNS

In the Cloudflare dashboard for each domain, add A records pointing to your Hetzner server IP with the proxy enabled (orange cloud):

| Type | Name | Content | Proxy |
|------|------|---------|-------|
| A | `@` | `<server IP>` | Proxied |
| A | `www` | `<server IP>` | Proxied |

Do this for every domain Vela serves (e.g. `cyanea.bio`, `archipelag.io`).

### 2. SSL/TLS Mode

Go to **SSL/TLS** and set the mode to **Full (Strict)**.

This means Cloudflare encrypts traffic to your origin server and validates the certificate. You need a valid origin cert on the server (next step).

### 3. Origin Certificate

Don't use Let's Encrypt when Cloudflare is proxying — HTTP-01 ACME challenges break because Cloudflare intercepts the validation request. Instead, use a **Cloudflare Origin Certificate**. It's free, lasts 15 years, and Cloudflare trusts it automatically.

Generate one:

1. **SSL/TLS** → **Origin Server** → **Create Certificate**
2. Add your domains (e.g. `*.cyanea.bio`, `cyanea.bio`, `*.archipelag.io`, `archipelag.io`)
3. Choose PEM format, 15 years
4. Save the certificate and private key

Put them on your server:

```bash
mkdir -p /etc/vela/tls
# Paste the certificate
nano /etc/vela/tls/origin.pem
# Paste the private key
nano /etc/vela/tls/origin-key.pem

chmod 600 /etc/vela/tls/origin-key.pem
```

Configure Vela to use them:

```toml
# /etc/vela/server.toml
[tls]
cert = "/etc/vela/tls/origin.pem"
key = "/etc/vela/tls/origin-key.pem"
```

No `acme_email` needed. Cloudflare handles public-facing TLS certificates.

### 4. Firewall

Since Cloudflare proxies all traffic, lock your server so only Cloudflare can reach ports 80 and 443. This prevents anyone from bypassing Cloudflare and hitting your server directly.

```bash
# Allow SSH
ufw allow 22/tcp

# Allow HTTP/HTTPS only from Cloudflare IPs
# Full list: https://www.cloudflare.com/ips/
for ip in \
    173.245.48.0/20 \
    103.21.244.0/22 \
    103.22.200.0/22 \
    103.31.4.0/22 \
    141.101.64.0/18 \
    108.162.192.0/18 \
    190.93.240.0/20 \
    188.114.96.0/20 \
    197.234.240.0/22 \
    198.41.128.0/17 \
    162.158.0.0/15 \
    104.16.0.0/13 \
    104.24.0.0/14 \
    172.64.0.0/13 \
    131.0.72.0/22; do
    ufw allow from $ip to any port 80,443 proto tcp
done

# Deny everything else
ufw default deny incoming
ufw enable
```

Cloudflare publishes their IP ranges at https://www.cloudflare.com/ips/. If they change, update the firewall rules.

### 5. Cloudflare Settings

Recommended baseline:

| Setting | Value | Location |
|---------|-------|----------|
| SSL mode | Full (Strict) | SSL/TLS |
| Minimum TLS version | 1.2 | SSL/TLS → Edge Certificates |
| Always Use HTTPS | On | SSL/TLS → Edge Certificates |
| Auto Minify | Off | Speed → Optimization |
| Brotli | On | Speed → Optimization |
| Browser Integrity Check | On | Security |

**Auto Minify** is off because it can break JavaScript and CSS. Your build tools already handle minification.

## Real IP Headers

When Cloudflare proxies requests, your app sees Cloudflare's IP as the client IP. The real client IP is in the `CF-Connecting-IP` header (and `X-Forwarded-For`).

If your app needs the real client IP (for logging, rate limiting, etc.), read from `CF-Connecting-IP`.

## Caching

Cloudflare caches static assets by default (images, CSS, JS, fonts). For API routes and dynamic pages, it passes through to your origin.

If you want to cache specific pages or API responses, use Cloudflare Cache Rules or set `Cache-Control` headers from your app.

## Alternative: DNS-Only Mode

If you don't want Cloudflare proxying (just DNS), set the proxy toggle to **DNS only** (gray cloud) on your A records. In this mode:

- Vela handles TLS directly via Let's Encrypt (ACME)
- Your server IP is exposed publicly
- No Cloudflare DDoS protection or caching
- Simpler setup, fewer moving parts

Configure Vela for ACME:

```toml
# /etc/vela/server.toml
[tls]
acme_email = "you@example.com"
```

For a small personal server with no traffic, DNS-only mode works fine. Switch to proxied mode if you want the extra protection.
