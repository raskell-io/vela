mod acme;
mod deploy;
mod ipc;
mod process;
mod proxy;
mod sandbox;
mod state;

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::cli::{
    AppsArgs, InternalDeployArgs, InternalLogsArgs, InternalRollbackArgs, InternalSecretAction,
    InternalSecretArgs, ServeArgs, SetupArgs,
};
use crate::config::{AppType, Manifest, ServerConfig};

/// Start the vela server daemon (proxy + process manager + IPC).
pub fn run(args: ServeArgs) -> Result<()> {
    // Install the rustls CryptoProvider before any TLS usage (ACME, cert loading, etc.)
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();

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
        let challenge_store = acme::ChallengeStore::new();
        let process_manager = Arc::new(Mutex::new(process::ProcessManager::new(config.logs_dir())));

        // Restore previously active apps
        restore_apps(&config, &state, &process_manager, &route_table).await?;

        // Set up ACME / dynamic TLS if configured
        let cert_resolver = if config.tls.acme_email.is_some() {
            let resolver = Arc::new(acme::CertResolver::new());

            // Load any existing certs from disk
            let active_apps = state.list_active_apps()?;
            let domains: Vec<String> = active_apps.iter().map(|a| a.domain.clone()).collect();

            for domain in &domains {
                let paths = acme::CertPaths::for_domain(&config.data_dir, domain);
                if paths.exists()
                    && let Err(e) = resolver.load_cert(domain, &paths.cert, &paths.key)
                {
                    tracing::warn!(domain, err = %e, "failed to load existing cert");
                }
            }

            // Spawn background cert provisioning
            let data_dir = config.data_dir.clone();
            let email = config.tls.acme_email.clone().unwrap();
            let staging = config.tls.staging;
            let cs = challenge_store.clone();
            let cr = resolver.clone();

            tokio::spawn(async move {
                // Initial provisioning
                if let Err(e) =
                    acme::provision_and_load_certs(&data_dir, &domains, &email, &cs, &cr, staging)
                        .await
                {
                    tracing::error!(err = %e, "ACME cert provisioning failed");
                }

                // Check for renewals every 12 hours
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(12 * 3600));
                interval.tick().await; // skip first immediate tick
                loop {
                    interval.tick().await;
                    tracing::debug!("checking for certificate renewals");
                    if let Err(e) = acme::provision_and_load_certs(
                        &data_dir, &domains, &email, &cs, &cr, staging,
                    )
                    .await
                    {
                        tracing::error!(err = %e, "ACME cert renewal check failed");
                    }
                }
            });

            Some(resolver)
        } else {
            None
        };

        // Start the reverse proxy
        let _proxy_handle = proxy::start_proxy(
            &config,
            route_table.clone(),
            challenge_store.clone(),
            cert_resolver,
        )?;

        // Start the IPC server (Unix socket for _deploy/_rollback communication)
        let sock_path = config.socket_path();
        let ipc_pm = process_manager.clone();
        let ipc_rt = route_table.clone();
        let ipc_sock = sock_path.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::start_ipc_server(&ipc_sock, ipc_pm, ipc_rt).await {
                tracing::error!(err = %e, "IPC server exited with error");
            }
        });

        // Spawn process supervision loop (check every 10s, restart crashed processes)
        let supervision_pm = process_manager.clone();
        let supervision_rt = route_table.clone();
        let supervision_state = state::ServerState::open(&config)?;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                let mut pm = supervision_pm.lock().await;
                let restarted = pm.check_and_restart().await;
                // Update route table for restarted apps
                for app_name in &restarted {
                    if let Some(port) = pm.active_port(app_name)
                        && let Ok(Some(app_config)) = supervision_state.get_app(app_name)
                    {
                        supervision_rt.set(&app_config.domain, port);
                    }
                }
            }
        });

        tracing::info!("vela server ready");

        // Wait for shutdown
        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");

        // Graceful shutdown: stop all apps
        let mut pm = process_manager.lock().await;
        let active_names: Vec<String> = pm
            .list_active()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();
        for name in &active_names {
            let _ = pm.stop(name).await;
        }

        // Clean up socket file
        let _ = std::fs::remove_file(&sock_path);

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

        // Merge env: manifest env vars first, then secrets override
        let secrets = state.get_secrets(&app.name)?;
        let mut env_vars: Vec<(String, String)> = app
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, value) in &secrets {
            if let Some(existing) = env_vars.iter_mut().find(|(k, _)| k == key) {
                existing.1 = value.clone();
            } else {
                env_vars.push((key.clone(), value.clone()));
            }
        }

        let mut pm = process_manager.lock().await;
        match pm
            .start(
                &app.name,
                &release_dir,
                binary_name,
                app_type,
                &data_dir,
                &env_vars,
            )
            .await
        {
            Ok(port) => {
                pm.promote_pending_to_active(&app.name);
                route_table.set(&app.domain, port);
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
/// then delegates process management to the daemon via IPC.
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
        manifest.env.clone(),
    )?;

    // Generate release ID and extract
    let release_id = deploy::generate_release_id();
    let release_dir = deploy::extract_release(&config.apps_dir(), app_name, &release_id, tarball)?;

    // Ensure data directory exists and sandbox the release
    let data_dir = deploy::ensure_data_dir(&config.apps_dir(), app_name)?;
    sandbox::prepare_sandbox(app_name, &release_dir, &data_dir)?;

    // Collect env vars (manifest env + secrets)
    let mut env_vars: Vec<(String, String)> = manifest
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Resolve ${secret:KEY} references and add secrets as env vars
    let secrets = state.get_secrets(app_name)?;
    for (_env_key, env_val) in &mut env_vars {
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

    // Run pre_start hook if configured
    if let Some(ref pre_start) = manifest.deploy.pre_start {
        eprintln!("  running pre_start hook...");
        let status = std::process::Command::new("sh")
            .args(["-c", pre_start])
            .current_dir(&release_dir)
            .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("VELA_DATA_DIR", &data_dir)
            .env("VELA_APP_NAME", app_name)
            .status()
            .context("failed to run pre_start hook")?;

        if !status.success() {
            let _ = sandbox::release_sandbox(&release_dir);
            let _ = std::fs::remove_dir_all(&release_dir);
            let _ = std::fs::remove_file(tarball);
            anyhow::bail!(
                "pre_start hook failed with exit code {}",
                status.code().unwrap_or(-1)
            );
        }
    }

    let post_deploy_cmd = manifest.deploy.post_deploy.clone();

    // Send deploy request to the daemon via IPC
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let sock_path = config.socket_path();

        eprintln!("  activating {app_name} (this may take up to 30s for health check)...");

        let response = ipc::send_command(
            &sock_path,
            &ipc::DaemonRequest::Deploy {
                app: app_name.to_string(),
                release_dir: release_dir.clone(),
                binary_name: binary_name.to_string(),
                app_type: app_type.as_str().to_string(),
                strategy: strategy.as_str().to_string(),
                data_dir: data_dir.clone(),
                env_vars: env_vars.clone(),
                health_path: health_path.map(String::from),
                drain_seconds: drain,
                domain: manifest.app.domain.clone(),
            },
        )
        .await?;

        if response.success {
            // Update symlink and clean up
            deploy::link_current(&config.apps_dir(), app_name, &release_id)?;

            let port = response.port.unwrap_or(0);
            eprintln!("  release {release_id} is active on port {port}");

            deploy::cleanup_old_releases(&config.apps_dir(), app_name, 5)?;

            // Run post_deploy hook if configured
            if let Some(ref post_deploy) = post_deploy_cmd {
                eprintln!("  running post_deploy hook...");
                let status = std::process::Command::new("sh")
                    .args(["-c", post_deploy])
                    .current_dir(&release_dir)
                    .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .env("VELA_DATA_DIR", &data_dir)
                    .env("VELA_APP_NAME", app_name)
                    .env("PORT", port.to_string())
                    .status();

                match status {
                    Ok(s) if s.success() => {
                        eprintln!("  post_deploy hook completed");
                    }
                    Ok(s) => {
                        eprintln!(
                            "  warning: post_deploy hook exited with code {}",
                            s.code().unwrap_or(-1)
                        );
                    }
                    Err(e) => {
                        eprintln!("  warning: post_deploy hook failed: {e}");
                    }
                }
            }
        } else {
            // Clean up failed release
            let _ = sandbox::release_sandbox(&release_dir);
            let _ = std::fs::remove_dir_all(&release_dir);
            let _ = std::fs::remove_file(tarball);
            anyhow::bail!("deploy failed: {}", response.message);
        }

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
    let data_dir = deploy::ensure_data_dir(&config.apps_dir(), app_name)?;
    let secrets = state.get_secrets(app_name)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let sock_path = config.socket_path();

        eprintln!("  rolling back {app_name} to {prev_release}...");

        let response = ipc::send_command(
            &sock_path,
            &ipc::DaemonRequest::Deploy {
                app: app_name.to_string(),
                release_dir: release_dir.clone(),
                binary_name: binary_name.to_string(),
                app_type: app.app_type.clone(),
                strategy: app.deploy_strategy.clone(),
                data_dir,
                env_vars: secrets,
                health_path: app.health_path.clone(),
                drain_seconds: app.drain_seconds,
                domain: app.domain.clone(),
            },
        )
        .await?;

        if response.success {
            deploy::link_current(&config.apps_dir(), app_name, &prev_release)?;
            let port = response.port.unwrap_or(0);
            eprintln!("  rolled back {app_name} to {prev_release} on port {port}");
        } else {
            anyhow::bail!("rollback failed: {}", response.message);
        }

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

/// Internal logs command — called by the client over SSH.
pub fn internal_logs(args: InternalLogsArgs) -> Result<()> {
    let config = ServerConfig::load(&args.config)?;
    let log_file = if args.stderr {
        "stderr.log"
    } else {
        "stdout.log"
    };
    let log_path = config.logs_dir().join(&args.app).join(log_file);

    if !log_path.exists() {
        anyhow::bail!("no logs found for '{}' ({})", args.app, log_path.display());
    }

    if args.follow {
        let status = std::process::Command::new("tail")
            .args(["-n", &args.lines.to_string(), "-f"])
            .arg(&log_path)
            .status()?;
        if !status.success() {
            anyhow::bail!("tail exited with {status}");
        }
    } else {
        let status = std::process::Command::new("tail")
            .args(["-n", &args.lines.to_string()])
            .arg(&log_path)
            .status()?;
        if !status.success() {
            anyhow::bail!("tail exited with {status}");
        }
    }

    Ok(())
}

/// Generate and install a systemd service file for `vela serve`.
pub fn setup(args: SetupArgs) -> Result<()> {
    let vela_bin = std::env::current_exe().context("failed to determine vela binary path")?;
    let vela_bin = vela_bin.display();
    let config_path = args.config.display();

    let unit = format!(
        r#"[Unit]
Description=Vela deployment server
After=network.target

[Service]
Type=simple
ExecStart={vela_bin} serve --config {config_path}
Restart=always
RestartSec=5
Environment=RUST_LOG=info

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/vela
PrivateTmp=true

[Install]
WantedBy=multi-user.target
"#
    );

    let service_path = Path::new("/etc/systemd/system/vela.service");

    // Check if we can write to systemd directory
    if service_path.parent().is_some_and(|p| p.exists()) {
        std::fs::write(service_path, &unit).context("failed to write systemd service file")?;
        println!("wrote {}", service_path.display());

        // Reload systemd
        let _ = std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status();

        println!("\nto start vela:");
        println!("  sudo systemctl enable --now vela");
    } else {
        // Not on a systemd system or no permissions — just print it
        println!("{unit}");
        println!("# save this to /etc/systemd/system/vela.service");
    }

    Ok(())
}
