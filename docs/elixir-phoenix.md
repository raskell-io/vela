# Deploying Elixir/Phoenix Apps

Vela supports Phoenix apps out of the box via BEAM releases.

## Build a Release

In your Phoenix project:

```bash
# Install dependencies and compile
MIX_ENV=prod mix deps.get
MIX_ENV=prod mix compile

# Build assets (if using esbuild/tailwind)
MIX_ENV=prod mix assets.deploy

# Build the release
MIX_ENV=prod mix release
```

This creates a self-contained release in `_build/prod/rel/my_app/` that includes the BEAM runtime. No Erlang or Elixir installation needed on the server.

## Vela.toml

```toml
[app]
name = "my-app"
domain = "my-app.example.com"

[deploy]
server = "root@your-server.example.com"
type = "beam"
binary = "bin/server"
health = "/health"
strategy = "sequential"    # Recommended for SQLite apps
drain = 10

[env]
DATABASE_PATH = "${data_dir}/my-app.db"
SECRET_KEY_BASE = "${secret:SECRET_KEY_BASE}"
PHX_HOST = "my-app.example.com"
PHX_SERVER = "true"
```

Key points:
- **`type = "beam"`** tells Vela this is a BEAM release, started with `bin/server start`
- **`strategy = "sequential"`** avoids two instances fighting over the SQLite database
- **`PHX_SERVER = "true"`** ensures the Phoenix endpoint starts (not just the app)

## Set Secrets

```bash
vela secret set my-app SECRET_KEY_BASE=$(mix phx.gen.secret)
```

## Deploy

```bash
MIX_ENV=prod mix release
vela deploy ./_build/prod/rel/my_app
```

## Health Check

Add a health check route to your router:

```elixir
# lib/my_app_web/router.ex
scope "/health", MyAppWeb do
  get "/", HealthController, :index
end
```

```elixir
# lib/my_app_web/controllers/health_controller.ex
defmodule MyAppWeb.HealthController do
  use MyAppWeb, :controller

  def index(conn, _params) do
    send_resp(conn, 200, "ok")
  end
end
```

Or as a simple plug:

```elixir
# lib/my_app_web/endpoint.ex
plug :health_check

defp health_check(%{request_path: "/health"} = conn, _opts) do
  conn |> send_resp(200, "ok") |> halt()
end
defp health_check(conn, _opts), do: conn
```

## Listening on the Right Port

Vela sets the `PORT` environment variable. Configure your endpoint to use it:

```elixir
# config/runtime.exs
config :my_app, MyAppWeb.Endpoint,
  http: [port: System.get_env("PORT") |> String.to_integer()],
  server: true
```

## SQLite with Ecto

Point your repo at the persistent data directory:

```elixir
# config/runtime.exs
config :my_app, MyApp.Repo,
  database: System.get_env("DATABASE_PATH", "my_app.db")
```

Set `DATABASE_PATH` in your `Vela.toml`:

```toml
[env]
DATABASE_PATH = "${data_dir}/my-app.db"
```

The `data/` directory persists across deploys. Your database is safe.

### Migrations

Run migrations on startup in your `application.ex`:

```elixir
def start(_type, _args) do
  MyApp.Release.migrate()
  # ... rest of supervision tree
end
```

Or in the release module:

```elixir
defmodule MyApp.Release do
  def migrate do
    for repo <- Application.fetch_env!(:my_app, :ecto_repos) do
      {:ok, _, _} = Ecto.Migrator.with_repo(repo, &Ecto.Migrator.run(&1, :up, all: true))
    end
  end
end
```

## Tips

- **WAL mode**: Enable SQLite WAL mode for better concurrent read performance. Set `journal_mode: :wal` in your repo config.
- **Graceful shutdown**: Phoenix handles `SIGTERM` by default. Vela sends `SIGTERM` during drain.
- **Logs**: `vela logs my-app -f` tails the app's stdout/stderr via journald.
