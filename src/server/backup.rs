//! Scheduled backup management.
//!
//! Backs up app data, secrets, and Postgres databases to a local directory
//! or S3-compatible storage.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::service;
use super::state::ServerState;
use crate::config::BackupConfig;

/// Run a backup cycle: collect data, compress, upload, enforce retention.
pub async fn run_backup(
    config: &BackupConfig,
    apps_dir: &Path,
    services_dir: &Path,
    state: &ServerState,
) -> Result<()> {
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let tmp_dir = std::env::temp_dir().join(format!("vela-backup-{timestamp}"));
    std::fs::create_dir_all(&tmp_dir)?;

    tracing::info!(timestamp = %timestamp, "starting backup");

    let mut files_backed_up = 0u32;

    // Back up app data directories
    if config.include.app_data {
        let apps = state.list_apps()?;
        for app in &apps {
            let data_dir = apps_dir.join(&app.name).join("data");
            if data_dir.exists() {
                let dest = tmp_dir.join("app_data").join(&app.name);
                std::fs::create_dir_all(&dest)?;

                // Checkpoint SQLite WAL files before copying
                checkpoint_sqlite_wals(&data_dir);

                copy_dir_recursive(&data_dir, &dest)?;
                files_backed_up += 1;
                tracing::info!(app = %app.name, "backed up app data");
            }
        }
    }

    // Back up secrets
    if config.include.secrets {
        let apps = state.list_apps()?;
        for app in &apps {
            let secrets_path = apps_dir.join(&app.name).join("secrets.env");
            if secrets_path.exists() {
                let dest = tmp_dir.join("secrets");
                std::fs::create_dir_all(&dest)?;
                std::fs::copy(&secrets_path, dest.join(format!("{}.env", app.name)))?;
                files_backed_up += 1;
            }
        }

        // Also back up app.toml configs
        for app in &apps {
            let config_path = apps_dir.join(&app.name).join("app.toml");
            if config_path.exists() {
                let dest = tmp_dir.join("configs");
                std::fs::create_dir_all(&dest)?;
                std::fs::copy(&config_path, dest.join(format!("{}.toml", app.name)))?;
            }
        }
    }

    // Back up Postgres databases
    if config.include.postgres {
        let pg_dest = tmp_dir.join("postgres");
        std::fs::create_dir_all(&pg_dest)?;
        match service::backup_postgres_databases(services_dir, &pg_dest) {
            Ok(paths) => {
                files_backed_up += paths.len() as u32;
            }
            Err(e) => {
                tracing::warn!(err = %e, "Postgres backup failed");
            }
        }
    }

    if files_backed_up == 0 {
        tracing::info!("nothing to back up");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Ok(());
    }

    // Create a tarball of the backup
    let tarball_name = format!("vela-backup-{timestamp}.tar.gz");
    let tarball_path = std::env::temp_dir().join(&tarball_name);

    let status = std::process::Command::new("tar")
        .args([
            "czf",
            &tarball_path.to_string_lossy(),
            "-C",
            &tmp_dir.to_string_lossy(),
            ".",
        ])
        .status()
        .context("failed to create backup tarball")?;

    if !status.success() {
        anyhow::bail!("tar failed creating backup");
    }

    // Upload to destination
    upload_backup(&tarball_path, &config.destination, &tarball_name)?;

    tracing::info!(
        destination = %config.destination,
        files = files_backed_up,
        "backup completed"
    );

    // Enforce retention
    enforce_retention(&config.destination, config.retain)?;

    // Clean up temp files
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_file(&tarball_path);

    Ok(())
}

/// Parse schedule string into an interval in seconds.
pub fn schedule_to_interval_secs(schedule: &str) -> u64 {
    match schedule {
        "hourly" => 3600,
        "daily" => 86400,
        s if s.ends_with('h') => {
            s.trim_end_matches('h')
                .parse::<u64>()
                .unwrap_or(24)
                * 3600
        }
        _ => 86400, // default to daily
    }
}

/// Upload a backup tarball to the destination.
fn upload_backup(tarball: &Path, destination: &str, name: &str) -> Result<()> {
    if destination.starts_with("s3://") {
        // S3-compatible upload via AWS CLI
        let dest = format!("{}/{}", destination.trim_end_matches('/'), name);
        let status = std::process::Command::new("aws")
            .args(["s3", "cp", &tarball.to_string_lossy(), &dest])
            .status()
            .context("failed to run 'aws s3 cp' — is the AWS CLI installed?")?;

        if !status.success() {
            anyhow::bail!("aws s3 cp failed for {dest}");
        }
    } else {
        // Local directory
        let dest_dir = Path::new(destination);
        std::fs::create_dir_all(dest_dir)?;
        std::fs::copy(tarball, dest_dir.join(name))?;
    }

    Ok(())
}

/// Enforce backup retention by removing old backups.
fn enforce_retention(destination: &str, retain: u32) -> Result<()> {
    if destination.starts_with("s3://") {
        // List and delete old backups via AWS CLI
        let output = std::process::Command::new("aws")
            .args([
                "s3",
                "ls",
                &format!("{}/", destination.trim_end_matches('/')),
            ])
            .output()
            .context("failed to list S3 backups")?;

        if !output.status.success() {
            return Ok(()); // Best effort
        }

        let listing = String::from_utf8_lossy(&output.stdout);
        let mut files: Vec<&str> = listing
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|f| f.starts_with("vela-backup-") && f.ends_with(".tar.gz"))
            .collect();

        files.sort();

        if files.len() > retain as usize {
            let to_delete = files.len() - retain as usize;
            for file in files.iter().take(to_delete) {
                let key = format!("{}/{}", destination.trim_end_matches('/'), file);
                let _ = std::process::Command::new("aws")
                    .args(["s3", "rm", &key])
                    .status();
                tracing::info!(file, "removed old backup from S3");
            }
        }
    } else {
        // Local directory
        let dest_dir = Path::new(destination);
        if !dest_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<PathBuf> = std::fs::read_dir(dest_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|f| {
                        let name = f.to_string_lossy();
                        name.starts_with("vela-backup-") && name.ends_with(".tar.gz")
                    })
            })
            .collect();

        entries.sort();

        if entries.len() > retain as usize {
            let to_delete = entries.len() - retain as usize;
            for path in entries.iter().take(to_delete) {
                let _ = std::fs::remove_file(path);
                tracing::info!(file = %path.display(), "removed old backup");
            }
        }
    }

    Ok(())
}

/// Checkpoint any SQLite WAL files in a directory for a consistent backup.
fn checkpoint_sqlite_wals(data_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "db") {
                let wal = path.with_extension("db-wal");
                if wal.exists() {
                    let _ = std::process::Command::new("sqlite3")
                        .arg(&path)
                        .arg("PRAGMA wal_checkpoint(TRUNCATE);")
                        .status();
                }
            }
        }
    }
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_parsing() {
        assert_eq!(schedule_to_interval_secs("hourly"), 3600);
        assert_eq!(schedule_to_interval_secs("daily"), 86400);
        assert_eq!(schedule_to_interval_secs("12h"), 43200);
        assert_eq!(schedule_to_interval_secs("6h"), 21600);
        assert_eq!(schedule_to_interval_secs("unknown"), 86400);
    }

    #[test]
    fn local_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().to_string_lossy().to_string();

        // Create 5 fake backups
        for i in 1..=5 {
            std::fs::write(
                tmp.path().join(format!("vela-backup-2026030{i}-120000.tar.gz")),
                "fake",
            )
            .unwrap();
        }

        enforce_retention(&dest, 2).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn copy_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("file.txt"), "hello").unwrap();
        std::fs::write(src.join("sub").join("nested.txt"), "world").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("sub").join("nested.txt")).unwrap(),
            "world"
        );
    }
}
