mod ssh;

use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::{
    DeployArgs, InitArgs, LogsArgs, RollbackArgs, SecretAction, SecretArgs, StatusArgs,
};
use crate::config::Manifest;

pub fn init(args: InitArgs) -> Result<()> {
    let name = args.name.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "myapp".to_string())
    });

    let domain = args.domain.unwrap_or_else(|| format!("{name}.example.com"));

    let content = format!(
        r#"[app]
name = "{name}"
domain = "{domain}"

[deploy]
server = "root@your-server.example.com"
type = "binary"
binary = "{name}"
health = "/health"
# strategy = "blue-green"  # or "sequential" for SQLite apps

[env]
# DATABASE_PATH = "${{data_dir}}/{name}.db"

[resources]
# memory = "512M"
"#
    );

    let path = Path::new("Vela.toml");
    if path.exists() {
        anyhow::bail!("Vela.toml already exists");
    }

    std::fs::write(path, &content)?;
    println!("created Vela.toml for '{name}'");
    println!("edit [deploy].server and run: vela deploy ./target/release/{name}");
    Ok(())
}

pub fn deploy(args: DeployArgs) -> Result<()> {
    let manifest = Manifest::load(&args.manifest)?;
    let server = args
        .server
        .or(manifest.deploy.server.clone())
        .context("no server specified (use --server or set deploy.server in Vela.toml)")?;

    let artifact = &args.artifact;
    if !artifact.exists() {
        anyhow::bail!("artifact not found: {}", artifact.display());
    }

    let app_name = &manifest.app.name;

    println!("deploying {app_name} to {server}");

    // 1. Create a tarball of the artifact
    let tarball = ssh::create_tarball(artifact, app_name)?;

    // 2. Upload via scp
    println!("  uploading artifact...");
    ssh::upload(&server, &tarball, app_name)?;

    // 3. Tell the server to activate this deploy
    println!("  activating...");
    let manifest_toml = std::fs::read_to_string(&args.manifest)?;
    ssh::activate(&server, app_name, &manifest_toml)?;

    println!("  deployed {app_name} to {server}");

    // Cleanup local tarball
    let _ = std::fs::remove_file(&tarball);

    Ok(())
}

pub fn status(args: StatusArgs) -> Result<()> {
    let server = resolve_server(args.server, &args.manifest)?;
    ssh::run_remote(&server, &["vela", "apps"])?;
    Ok(())
}

pub fn logs(args: LogsArgs) -> Result<()> {
    let server = resolve_server(args.server, &args.manifest)?;
    let unit = format!("vela-app-{}", args.app);
    let lines = args.lines.to_string();
    let mut cmd = vec!["journalctl", "-u", &unit, "-n", &lines];
    if args.follow {
        cmd.push("-f");
    }
    ssh::run_remote_interactive(&server, &cmd)?;
    Ok(())
}

pub fn rollback(args: RollbackArgs) -> Result<()> {
    let server = resolve_server(args.server, &args.manifest)?;
    let app_name = args
        .app
        .or_else(|| Manifest::load(&args.manifest).ok().map(|m| m.app.name))
        .context("specify app name or have a Vela.toml")?;

    println!("rolling back {app_name} on {server}");
    ssh::run_remote(&server, &["vela", "app", &app_name, "rollback"])?;
    Ok(())
}

pub fn secret(args: SecretArgs) -> Result<()> {
    match args.action {
        SecretAction::Set {
            app,
            pair,
            server,
            manifest,
        } => {
            let server = resolve_server(server, &manifest)?;
            ssh::run_remote(&server, &["vela", "secret", "set", &app, &pair])?;
        }
        SecretAction::List {
            app,
            server,
            manifest,
        } => {
            let server = resolve_server(server, &manifest)?;
            ssh::run_remote(&server, &["vela", "secret", "list", &app])?;
        }
        SecretAction::Remove {
            app,
            key,
            server,
            manifest,
        } => {
            let server = resolve_server(server, &manifest)?;
            ssh::run_remote(&server, &["vela", "secret", "remove", &app, &key])?;
        }
    }
    Ok(())
}

fn resolve_server(server: Option<String>, manifest_path: &Path) -> Result<String> {
    if let Some(s) = server {
        return Ok(s);
    }
    let manifest = Manifest::load(manifest_path)?;
    manifest
        .deploy
        .server
        .context("no server specified (use --server or set deploy.server in Vela.toml)")
}
