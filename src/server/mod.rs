mod deploy;
mod process;
mod proxy;
mod state;

use anyhow::Result;

use crate::cli::{AppsArgs, ServeArgs};
use crate::config::ServerConfig;

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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let state = state::ServerState::open(&config)?;

        // TODO: Start proxy (Pingora)
        // TODO: Start process manager (restore running apps)
        // TODO: Wait for shutdown signal

        tracing::info!("vela server ready");

        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");

        Ok(())
    })
}

pub fn apps(args: AppsArgs) -> Result<()> {
    let config = ServerConfig::default();
    let state = state::ServerState::open(&config)?;
    let apps = state.list_apps()?;

    if apps.is_empty() {
        println!("no apps deployed");
        return Ok(());
    }

    for app in &apps {
        if args.verbose {
            println!(
                "{:<20} {:<30} {:<15} {}",
                app.name, app.domain, app.current_release, app.status
            );
        } else {
            println!("{:<20} {}", app.name, app.domain);
        }
    }

    Ok(())
}
