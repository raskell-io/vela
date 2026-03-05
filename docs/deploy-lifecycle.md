# Deploy Lifecycle

Every deploy follows the same sequence. Understanding it helps you debug issues and write apps that work well with Vela.

## The Deploy Sequence

```
vela deploy ./target/release/my-app
│
├─ 1. Read Vela.toml
│     Parse manifest, resolve server address
│
├─ 2. Create tarball
│     tar czf the artifact (file or directory)
│
├─ 3. Upload via scp
│     scp tarball → server:/tmp/vela-deploy-<app>.tar.gz
│
├─ 4. Activate (ssh → server)
│     │
│     ├─ 4a. Generate release ID (timestamp: 20260305-143022)
│     ├─ 4b. Extract tarball → /var/vela/apps/<app>/releases/<id>/
│     ├─ 4c. Register app in database (upsert)
│     ├─ 4d. Start new instance on random port
│     ├─ 4e. Run health check (retry up to 30 times, 1s apart)
│     │
│     ├─ On health check success:
│     │   ├─ 4f. Update proxy routing (domain → new port)
│     │   ├─ 4g. Update current symlink → new release
│     │   ├─ 4h. Drain old instance (wait drain_seconds)
│     │   ├─ 4i. Stop old instance
│     │   └─ 4j. Clean up old releases (keep last 5)
│     │
│     └─ On health check failure:
│         ├─ Kill new instance
│         ├─ Mark release as failed
│         └─ Old instance keeps running (nothing changed)
│
└─ 5. Done
      Print success or failure
```

## File System After Deploy

```
/var/vela/apps/my-app/
├── app.toml                      # Parsed from Vela.toml
├── data/                         # Persistent (never touched by deploys)
│   └── my-app.db                 # Your SQLite database
├── releases/
│   ├── 20260305-140000/          # Previous release (kept for rollback)
│   │   └── my-app
│   └── 20260305-143022/          # Current release
│       └── my-app
└── current -> releases/20260305-143022
```

## Blue-Green vs Sequential

### Blue-Green (default)

```
Time ──────────────────────────────────────────►

Old instance     ████████████████████░░░░  (draining, then stopped)
New instance              ░░░░████████████████████
                          ▲   ▲
                     start │   │ health check passes,
                           │   traffic swaps
                           │
                      health checking
```

Both instances run briefly. Zero downtime. Use for stateless apps.

### Sequential

```
Time ──────────────────────────────────────────►

Old instance     ████████████████████
New instance                          ░░░░████████████████████
                                 ▲   ▲
                            stop │   │ start + health check
                                 │
                            ~1s blip
```

Old stops before new starts. Sub-second downtime. Use for SQLite apps.

## Health Checks

Your app must expose an HTTP endpoint that returns 200 when ready.

```
GET http://localhost:{PORT}/health → 200 OK
```

Vela checks this endpoint:
- **Interval**: every 1 second
- **Timeout**: 5 seconds per attempt
- **Retries**: 30 attempts (30 seconds total)

If your app needs more startup time, this will be configurable in a future version.

### Tips

- Don't return 200 until your app is actually ready (DB migrations done, caches warmed)
- For Phoenix apps, use `Phoenix.Endpoint.HealthCheck` or a simple plug
- For Rust apps, a basic `/health` route returning `200 OK` is enough

## Rollback

Rolling back switches to the previous release:

```bash
vela rollback my-app
```

This reactivates the previous release directory, restarts the process, and swaps the proxy. Same health check flow applies.

## What Your App Needs

1. **Listen on `$PORT`** — Vela assigns a random port via the `PORT` env var
2. **Health endpoint** — Return 200 at the path you configure in `Vela.toml`
3. **Graceful shutdown** — Handle `SIGTERM` to drain in-flight requests
4. **Use `$VELA_DATA_DIR`** — Store SQLite databases and persistent files here, not in the release directory
