# Architecture Rules

---

## Single Binary

Vela is one binary. Server and client share code through modules, not crates. No workspace, no sub-crates. Keep it simple.

## No Containers

Apps are processes, not containers. Isolation uses:
- Linux namespaces (PID, mount, network) via `nix` crate — future
- cgroups v2 for resource limits — future
- systemd sandboxing (PrivateTmp, ProtectSystem) — v1
- Separate Unix user per app — v1

## No Docker Dependency

Vela must never shell out to Docker, podman, or any container runtime. No OCI images. No Dockerfiles. Deploy artifacts are:
- A directory of files (binary, assets, config)
- Uploaded as a tarball, extracted on server

## SSH as Control Plane

- Client commands SSH into the server and run `vela` subcommands
- No custom daemon API, no REST endpoints, no gRPC
- Authentication = SSH key auth. If you can SSH, you can deploy.
- Upload artifacts via scp/rsync over SSH

## Proxy

- Pingora embedded in the server binary
- Handles TLS termination via Let's Encrypt (ACME)
- Routes by domain → app
- Health-check aware: only routes to healthy instances

## State

- Server state is filesystem-backed (no database)
- App configs in `app.toml`, secrets in `secrets.env`, releases as directories
- Active release tracked by `current` symlink
- Inspectable with standard Unix tools (ls, cat, readlink)

## Deploy Flow Invariants

1. A deploy never mutates a previous release directory
2. `current` symlink is the atomic switch point
3. Health check must pass before traffic switches
4. Failed deploys leave the old release running
5. At least 2 releases are kept for rollback (configurable)

## File Ownership

| Path | Owner |
|------|-------|
| `/var/vela/` | root or vela user |
| `/var/vela/apps/<app>/data/` | app-specific user |
| `/var/vela/apps/<app>/releases/` | vela user |
| `/var/vela/secrets/` | root, mode 0600 |

## Platform

- Server: Linux only (bare metal or dedicated server)
- Client: macOS, Linux (wherever you develop)
- No Windows server support
