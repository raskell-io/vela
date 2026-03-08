use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "vela",
    about = "No-downtime app deployment on bare metal",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the vela server (proxy + process manager)
    Serve(ServeArgs),

    /// Initialize a Vela.toml for a project
    Init(InitArgs),

    /// Deploy an app to a remote server
    Deploy(DeployArgs),

    /// Show status of apps on a remote server
    Status(StatusArgs),

    /// Tail logs from a remote app
    Logs(LogsArgs),

    /// Roll back an app to its previous release
    Rollback(RollbackArgs),

    /// Manage secrets for an app
    Secret(SecretArgs),

    /// List running apps (server-side)
    Apps(AppsArgs),

    // -----------------------------------------------------------------------
    // Internal commands (called by the client over SSH, not user-facing)
    // -----------------------------------------------------------------------
    /// [internal] Activate a deploy on the server
    #[command(hide = true)]
    #[clap(name = "_deploy")]
    InternalDeploy(InternalDeployArgs),

    /// [internal] Rollback an app on the server
    #[command(hide = true)]
    #[clap(name = "_rollback")]
    InternalRollback(InternalRollbackArgs),

    /// [internal] Manage secrets on the server
    #[command(hide = true)]
    #[clap(name = "_secret")]
    InternalSecret(InternalSecretArgs),

    /// [internal] Read app logs on the server
    #[command(hide = true)]
    #[clap(name = "_logs")]
    InternalLogs(InternalLogsArgs),

    /// Generate a systemd service file for vela
    Setup(SetupArgs),

    /// Run a backup now
    Backup(BackupArgs),

    /// [internal] Run backup on the server
    #[command(hide = true)]
    #[clap(name = "_backup")]
    InternalBackup(InternalBackupArgs),
}

// ---------------------------------------------------------------------------
// Server commands
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}

#[derive(clap::Args)]
pub struct AppsArgs {
    /// Show detailed info
    #[arg(short, long)]
    pub verbose: bool,

    /// Output as JSON (queries running daemon for live process info)
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Client commands
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct InitArgs {
    /// App name
    #[arg(short, long)]
    pub name: Option<String>,

    /// Domain for the app
    #[arg(short, long)]
    pub domain: Option<String>,
}

#[derive(clap::Args)]
pub struct DeployArgs {
    /// Path to the artifact (binary, directory, or tarball)
    pub artifact: PathBuf,

    /// Path to Vela.toml (defaults to ./Vela.toml)
    #[arg(short, long, default_value = "Vela.toml")]
    pub manifest: PathBuf,

    /// Override the server address
    #[arg(short, long)]
    pub server: Option<String>,
}

#[derive(clap::Args)]
pub struct StatusArgs {
    /// Server address (user@host)
    #[arg(short, long)]
    pub server: Option<String>,

    /// Path to Vela.toml (to infer server)
    #[arg(short, long, default_value = "Vela.toml")]
    pub manifest: PathBuf,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct LogsArgs {
    /// App name
    pub app: String,

    /// Server address (user@host)
    #[arg(short, long)]
    pub server: Option<String>,

    /// Number of lines to show
    #[arg(short = 'n', long, default_value = "100")]
    pub lines: u32,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Path to Vela.toml (to infer server)
    #[arg(short, long, default_value = "Vela.toml")]
    pub manifest: PathBuf,
}

#[derive(clap::Args)]
pub struct RollbackArgs {
    /// App name (optional if Vela.toml exists)
    pub app: Option<String>,

    /// Server address (user@host)
    #[arg(short, long)]
    pub server: Option<String>,

    /// Path to Vela.toml
    #[arg(short, long, default_value = "Vela.toml")]
    pub manifest: PathBuf,
}

#[derive(clap::Args)]
pub struct SecretArgs {
    #[command(subcommand)]
    pub action: SecretAction,
}

#[derive(Subcommand)]
pub enum SecretAction {
    /// Set a secret: vela secret set <app> KEY=VALUE
    Set {
        /// App name
        app: String,
        /// KEY=VALUE pair
        pair: String,
        /// Server address
        #[arg(short, long)]
        server: Option<String>,
        /// Path to Vela.toml
        #[arg(short, long, default_value = "Vela.toml")]
        manifest: PathBuf,
    },
    /// List secrets for an app
    List {
        /// App name
        app: String,
        /// Server address
        #[arg(short, long)]
        server: Option<String>,
        /// Path to Vela.toml
        #[arg(short, long, default_value = "Vela.toml")]
        manifest: PathBuf,
    },
    /// Remove a secret
    Remove {
        /// App name
        app: String,
        /// Secret key to remove
        key: String,
        /// Server address
        #[arg(short, long)]
        server: Option<String>,
        /// Path to Vela.toml
        #[arg(short, long, default_value = "Vela.toml")]
        manifest: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Internal commands (server-side, called via SSH)
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct InternalDeployArgs {
    /// App name
    pub app: String,

    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}

#[derive(clap::Args)]
pub struct InternalRollbackArgs {
    /// App name
    pub app: String,

    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}

#[derive(clap::Args)]
pub struct InternalSecretArgs {
    #[command(subcommand)]
    pub action: InternalSecretAction,
}

#[derive(Subcommand)]
pub enum InternalSecretAction {
    /// Set a secret
    Set {
        app: String,
        pair: String,
        #[arg(short, long, default_value = "/etc/vela/server.toml")]
        config: PathBuf,
    },
    /// List secret keys
    List {
        app: String,
        #[arg(short, long, default_value = "/etc/vela/server.toml")]
        config: PathBuf,
    },
    /// Remove a secret
    Remove {
        app: String,
        key: String,
        #[arg(short, long, default_value = "/etc/vela/server.toml")]
        config: PathBuf,
    },
}

#[derive(clap::Args)]
pub struct InternalLogsArgs {
    /// App name
    pub app: String,

    /// Number of lines to show
    #[arg(short = 'n', long, default_value = "100")]
    pub lines: u32,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Show stderr instead of stdout
    #[arg(long)]
    pub stderr: bool,

    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}

#[derive(clap::Args)]
pub struct SetupArgs {
    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}

#[derive(clap::Args)]
pub struct BackupArgs {
    /// Server address (user@host)
    #[arg(short, long)]
    pub server: Option<String>,

    /// Path to Vela.toml (to infer server)
    #[arg(short, long, default_value = "Vela.toml")]
    pub manifest: PathBuf,
}

#[derive(clap::Args)]
pub struct InternalBackupArgs {
    /// Path to server config file
    #[arg(short, long, default_value = "/etc/vela/server.toml")]
    pub config: PathBuf,
}
