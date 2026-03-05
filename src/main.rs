// Server code compiles on all platforms but is only used on Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::Result;
use clap::Parser;

mod cli;
mod config;
mod health;

#[cfg(target_os = "linux")]
mod server;

mod client;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        // Server commands (Linux only)
        #[cfg(target_os = "linux")]
        cli::Command::Serve(args) => server::run(args),

        #[cfg(not(target_os = "linux"))]
        cli::Command::Serve(_) => {
            anyhow::bail!("vela serve is only supported on Linux")
        }

        // Client commands (any platform)
        cli::Command::Init(args) => client::init(args),
        cli::Command::Deploy(args) => client::deploy(args),
        cli::Command::Status(args) => client::status(args),
        cli::Command::Logs(args) => client::logs(args),
        cli::Command::Rollback(args) => client::rollback(args),
        cli::Command::Secret(args) => client::secret(args),

        // Local server management (Linux only)
        #[cfg(target_os = "linux")]
        cli::Command::Apps(args) => server::apps(args),

        #[cfg(not(target_os = "linux"))]
        cli::Command::Apps(_) => {
            anyhow::bail!("vela apps is only supported on Linux")
        }
    }
}
