mod deploy;
mod process;
mod proxy;
mod state;

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::cli::{
    AppsArgs, InternalDeployArgs, InternalRollbackArgs, InternalSecretAction, InternalSecretArgs,
    ServeArgs,
};
use crate::config::{AppType, DeployStrategy, Manifest, ServerConfig};
use crate::health::HealthCheck;

/// Start the vela server daemon (proxy + process manager).
pub fn run(args: ServeArgs) -> Result<()> {
    let config = ServerConfig::load(&args.config)?;

    tracing::info!(
        data_dir = %config.data_dir.display(),
        http_port = config.proxy.http_port,
        https_port = config.proxy.https_port,
        "starting vela server"
    );

    // Ensure directories exist
    std::fs::create_dir_all(config.apps_dir())?;
    std::fs::create_dir_all(config.secrets_dir())?;
    std::fs::create_dir_all(config.logs_dir())?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let state = state::ServerState::open(&config)?;
        let route_table = proxy::RouteTable::new();
        let process_manager = Arc::new(Mutex::new(process::ProcessManager::new()));

        // Restore previously active apps
        restore_apps(&config, &state, &process_manager, &route_table).await?;

        // Start the reverse proxy
        let proxy_handle = proxy::start_proxy(&config, route_table.clone())?;

        tracing::info!("vela server ready");

        // Wait for shutdown
        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");

        // Graceful shutdown: stop all apps
        let mut pm = process_manager.lock().await;
        for (app_name, _port) in pm.list_active() {
            let name = app_name.to_string();
            let _ = pm.stop(&name).await;
        }

        Ok(())
    })
}

/// Restore apps that were active before a restart.
async fn restore_apps(
    config: &ServerConfig,
    state: &state::ServerState,
    process_manager: &Arc<Mutex<process::ProcessManager>>,
    route_table: &proxy::RouteTable,
) -> Result<()> {
    let active_apps = state.list_active_apps()?;

    for app in &active_apps {
        let release_id = match state.get_active_release(&app.name)? {
            Some(id) => id,
            None => continue,
        };

        let release_dir = config
            .apps_dir()
            .join(&app.name)
            .join("releases")
            .join(&release_id);

        if !release_dir.exists() {
            tracing::warn!(app = %app.name, release = %release_id, "release directory missing, skipping restore");
            continue;
        }

        let binary_name = app.binary_name.as_deref().unwrap_or(&app.name);
        let app_type = AppType::from_str_loose(&app.app_type);
        let data_dir = deploy::ensure_data_dir(&config.apps_dir(), &app.name)?;
        let secrets = state.get_secrets(&app.name)?;

        let mut pm = process_manager.lock().await;
        match pm
            .start(
                &app.name,
                &release_dir,
                binary_name,
                app_type,
                &data_dir,
                &secrets,
            )
            .await
        {
            Ok(port) => {
                // Promote directly to active (no pending/swap for restore)
                pm.promote_pending_to_active(&app.name);
                route_table.set(&app.domain, port);
                state.update_app_port(&app.name, port)?;
                tracing::info!(app = %app.name, port, release = %release_id, "restored app");
            }
            Err(e) => {
                tracing::error!(app = %app.name, err = %e, "failed to restore app");
            }
        }
    }

    if !active_apps.is_empty() {
        tracing::info!(count = active_apps.len(), "restored active apps");
    }

    Ok(())
}

/// List apps (server-side CLI command).
pub fn apps(args: AppsArgs) -> Result<()> {
    let config = ServerConfig::default();
    let state = state::ServerState::open(&config)?;
    let apps = state.list_apps()?;

    if apps.is_empty() {
        println!("no apps deployed");
        return Ok(());
    }

    if args.verbose {
        println!(
            "{:<20} {:<30} {:<20} {}",
            "NAME", "DOMAIN", "RELEASE", "STATUS"
        );
        println!("{}", "-".repeat(85));
    }

    for app in &apps {
        if args.verbose {
            println!(
                "{:<20} {:<30} {:<20} {}",
                app.name, app.domain, app.current_release, app.status
            );
        } else {
            println!("{:<20} {}", app.name, app.domain);
        }
    }

    Ok(())
}

/// Internal deploy command — called by the client over SSH.
/// Reads the manifest from stdin, extracts the pre-uploaded tarball,
/// starts the new instance, health checks, and swaps.
pub fn internal_deploy(args: InternalDeployArgs) -> Result<()> {
    let config = ServerConfig::load(&args.config)?;
    let app_name = &args.app;

    // Read manifest from stdin
    let mut manifest_str = String::new();
    std::io::stdin()
        .read_to_string(&mut manifest_str)
        .context("failed to read manifest from stdin")?;

    let manifest = Manifest::from_toml_str(&manifest_str)?;

    // Find the uploaded tarball
    let tarball_path = format!("/tmp/vela-deploy-{app_name}.tar.gz");
    let tarball = Path::new(&tarball_path);
    if !tarball.exists() {
        anyhow::bail!("tarball not found at {tarball_path} — was the upload successful?");
    }

    let state = state::ServerState::open(&config)?;

    // Register/update the app
    let app_type = manifest.deploy.r#type;
    let strategy = manifest.deploy.strategy;
    let binary_name = manifest
        .deploy
        .binary
        .as_deref()
        .unwrap_or(&manifest.app.name);
    let health_path = manifest.deploy.health.as_deref();
    let drain = manifest.deploy.drain;

    state.register_app(
        app_name,
        &manifest.app.domain,
        app_type.as_str(),
        Some(binary_name),
        health_path,
        strategy.as_str(),
        drain,
    )?;

    // Generate release ID and extract
    let release_id = deploy::generate_release_id();
    let release_dir = deploy::extract_release(&config.apps_dir(), app_name, &release_id, tarball)?;

    // Record release in DB
    state.create_release(app_name, &release_id)?;

    // Ensure data directory exists
    let data_dir = deploy::ensure_data_dir(&config.apps_dir(), app_name)?;

    // Collect env vars (manifest env + secrets)
    let mut env_vars: Vec<(String, String)> = manifest
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Resolve ${secret:KEY} references and add secrets as env vars
    let secrets = state.get_secrets(app_name)?;
    for (env_key, env_val) in &mut env_vars {
        for (secret_key, secret_val) in &secrets {
            let placeholder = format!("${{secret:{secret_key}}}");
            if env_val.contains(&placeholder) {
                *env_val = env_val.replace(&placeholder, secret_val);
            }
        }
    }
    // Also inject secrets directly as env vars
    for (key, value) in &secrets {
        if !env_vars.iter().any(|(k, _)| k == key) {
            env_vars.push((key.clone(), value.clone()));
        }
    }

    // Run the deploy in a tokio runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let mut pm = process::ProcessManager::new();

        // If sequential strategy, we don't start the new one until old is stopped.
        // For blue-green, we start new alongside old.

        // Start new instance
        let port = pm
            .start(
                app_name,
                &release_dir,
                binary_name,
                app_type,
                &data_dir,
                &env_vars,
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to start app: {e}"))?;

        eprintln!("  started {app_name} on port {port}, checking health...");

        // Health check
        if let Some(health) = health_path {
            let url = format!("http://127.0.0.1:{port}{health}");
            let hc = HealthCheck::new(url);
            match hc.wait_until_healthy().await {
                Ok(()) => {
                    eprintln!("  health check passed");
                }
                Err(e) => {
                    eprintln!("  health check failed: {e}");
                    pm.abort_pending(app_name).await?;
                    state.fail_release(app_name, &release_id)?;
                    // Clean up tarball
                    let _ = std::fs::remove_file(tarball);
                    anyhow::bail!("deploy failed: health check did not pass");
                }
            }
        } else {
            // No health check — wait a brief moment and assume OK
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            eprintln!("  no health check configured, assuming healthy");
        }

        // Activate: update symlink, DB, swap process
        deploy::link_current(&config.apps_dir(), app_name, &release_id)?;
        state.activate_release(app_name, &release_id)?;
        state.update_app_port(app_name, port)?;

        // For sequential: old was already stopped. For blue-green: we'd swap here.
        // Since _deploy runs as a one-shot (not inside the daemon), we promote directly.
        pm.promote_pending_to_active(app_name);

        eprintln!("  release {release_id} is active on port {port}");

        // Clean up old releases (keep 5)
        deploy::cleanup_old_releases(&config.apps_dir(), app_name, 5)?;

        // Clean up tarball
        let _ = std::fs::remove_file(tarball);

        Ok(())
    })
}

/// Internal rollback command — called by the client over SSH.
pub fn internal_rollback(args: InternalRollbackArgs) -> Result<()> {
    let config = ServerConfig::load(&args.config)?;
    let state = state::ServerState::open(&config)?;
    let app_name = &args.app;

    let app = state
        .get_app(app_name)?
        .context(format!("app '{app_name}' not found"))?;

    let prev_release = state
        .get_previous_release(app_name)?
        .context(format!("no previous release for '{app_name}'"))?;

    let release_dir = config
        .apps_dir()
        .join(app_name)
        .join("releases")
        .join(&prev_release);

    if !release_dir.exists() {
        anyhow::bail!(
            "previous release directory not found: {}",
            release_dir.display()
        );
    }

    let binary_name = app.binary_name.as_deref().unwrap_or(app_name);
    let app_type = AppType::from_str_loose(&app.app_type);
    let data_dir = deploy::ensure_data_dir(&config.apps_dir(), app_name)?;
    let secrets = state.get_secrets(app_name)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let mut pm = process::ProcessManager::new();

        let port = pm
            .start(
                app_name,
                &release_dir,
                binary_name,
                app_type,
                &data_dir,
                &secrets,
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to start app: {e}"))?;

        // Health check
        if let Some(health) = &app.health_path {
            let url = format!("http://127.0.0.1:{port}{health}");
            let hc = HealthCheck::new(url);
            hc.wait_until_healthy()
                .await
                .map_err(|e| anyhow::anyhow!("rollback health check failed: {e}"))?;
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        deploy::link_current(&config.apps_dir(), app_name, &prev_release)?;
        state.activate_release(app_name, &prev_release)?;
        state.update_app_port(app_name, port)?;
        pm.promote_pending_to_active(app_name);

        eprintln!("rolled back {app_name} to {prev_release} on port {port}");
        Ok(())
    })
}

/// Internal secret management — called by the client over SSH.
pub fn internal_secret(args: InternalSecretArgs) -> Result<()> {
    match args.action {
        InternalSecretAction::Set { app, pair, config } => {
            let config = ServerConfig::load(&config)?;
            let state = state::ServerState::open(&config)?;

            let (key, value) = pair.split_once('=').context("expected KEY=VALUE format")?;

            state.set_secret(&app, key, value)?;
            println!("set secret {key} for {app}");
        }
        InternalSecretAction::List { app, config } => {
            let config = ServerConfig::load(&config)?;
            let state = state::ServerState::open(&config)?;
            let secrets = state.get_secrets(&app)?;

            if secrets.is_empty() {
                println!("no secrets for {app}");
            } else {
                for (key, _) in &secrets {
                    println!("{key}");
                }
            }
        }
        InternalSecretAction::Remove { app, key, config } => {
            let config = ServerConfig::load(&config)?;
            let state = state::ServerState::open(&config)?;

            if state.remove_secret(&app, &key)? {
                println!("removed secret {key} from {app}");
            } else {
                println!("secret {key} not found for {app}");
            }
        }
    }
    Ok(())
}
