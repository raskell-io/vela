use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("app '{0}' not found")]
    AppNotFound(String),
    #[error("release directory not found: {0}")]
    ReleaseNotFound(String),
    #[error("health check failed: {0}")]
    HealthCheckFailed(String),
    #[error(transparent)]
    Process(#[from] super::process::ProcessError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Generate a release ID based on current timestamp.
pub fn generate_release_id() -> String {
    let now = chrono::Utc::now();
    now.format("%Y%m%d-%H%M%S").to_string()
}

/// Extract an uploaded tarball into the release directory.
pub fn extract_release(
    apps_dir: &Path,
    app_name: &str,
    release_id: &str,
    tarball: &Path,
) -> Result<PathBuf, DeployError> {
    let release_dir = apps_dir.join(app_name).join("releases").join(release_id);

    std::fs::create_dir_all(&release_dir)?;

    // Extract tarball
    let status = std::process::Command::new("tar")
        .args([
            "xzf",
            &tarball.to_string_lossy(),
            "-C",
            &release_dir.to_string_lossy(),
        ])
        .status()?;

    if !status.success() {
        return Err(DeployError::ReleaseNotFound(format!(
            "failed to extract tarball to {}",
            release_dir.display()
        )));
    }

    tracing::info!(
        app = app_name,
        release = release_id,
        dir = %release_dir.display(),
        "extracted release"
    );

    Ok(release_dir)
}

/// Update the `current` symlink to point to the given release.
pub fn link_current(apps_dir: &Path, app_name: &str, release_id: &str) -> Result<(), DeployError> {
    let app_dir = apps_dir.join(app_name);
    let current = app_dir.join("current");
    let target = app_dir.join("releases").join(release_id);

    // Remove old symlink if it exists
    if current.exists() || current.is_symlink() {
        std::fs::remove_file(&current)?;
    }

    std::os::unix::fs::symlink(&target, &current)?;

    tracing::info!(
        app = app_name,
        release = release_id,
        "updated current symlink"
    );

    Ok(())
}

/// Ensure the persistent data directory exists for an app.
pub fn ensure_data_dir(apps_dir: &Path, app_name: &str) -> Result<PathBuf, DeployError> {
    let data_dir = apps_dir.join(app_name).join("data");
    std::fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

/// Clean up old releases, keeping the N most recent.
pub fn cleanup_old_releases(
    apps_dir: &Path,
    app_name: &str,
    keep: usize,
) -> Result<(), DeployError> {
    let releases_dir = apps_dir.join(app_name).join("releases");
    if !releases_dir.exists() {
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&releases_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    // Sort by name (timestamp-based, so lexicographic = chronological)
    entries.sort_by_key(|e| e.file_name());

    if entries.len() <= keep {
        return Ok(());
    }

    let to_remove = entries.len() - keep;
    for entry in entries.iter().take(to_remove) {
        tracing::info!(
            app = app_name,
            release = ?entry.file_name(),
            "removing old release"
        );
        // Restore write permissions (sandbox makes release read-only)
        super::sandbox::release_sandbox(&entry.path())?;
        std::fs::remove_dir_all(entry.path())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_id_format() {
        let id = generate_release_id();
        // Should be YYYYMMDD-HHMMSS
        assert_eq!(id.len(), 15);
        assert_eq!(&id[8..9], "-");
    }

    #[test]
    fn ensure_data_dir_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = ensure_data_dir(tmp.path(), "testapp").unwrap();
        assert!(data_dir.exists());
        assert_eq!(data_dir, tmp.path().join("testapp").join("data"));
    }

    #[test]
    fn link_current_creates_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let app_dir = tmp.path().join("myapp");
        let release_dir = app_dir.join("releases").join("20260305-001");
        std::fs::create_dir_all(&release_dir).unwrap();

        link_current(tmp.path(), "myapp", "20260305-001").unwrap();

        let current = app_dir.join("current");
        assert!(current.is_symlink());
        assert_eq!(std::fs::read_link(&current).unwrap(), release_dir);
    }

    #[test]
    fn cleanup_keeps_recent_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let releases_dir = tmp.path().join("myapp").join("releases");

        for i in 1..=5 {
            std::fs::create_dir_all(releases_dir.join(format!("20260305-00{i}"))).unwrap();
        }

        cleanup_old_releases(tmp.path(), "myapp", 2).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(&releases_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();

        assert_eq!(remaining.len(), 2);
    }
}
