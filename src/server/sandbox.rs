use std::path::Path;

/// Apply filesystem restrictions before spawning an app process.
///
/// This sets up a basic sandbox using standard Linux permissions.
/// PID/mount namespaces and cgroups require additional privileges
/// and will be added in a future version.
pub fn prepare_sandbox(app_name: &str, release_dir: &Path, data_dir: &Path) -> std::io::Result<()> {
    // Make release directory read-only (app shouldn't mutate its own release)
    set_readonly_recursive(release_dir)?;

    // Ensure data directory is writable
    std::fs::create_dir_all(data_dir)?;

    tracing::debug!(app = app_name, "sandbox prepared");
    Ok(())
}

fn set_readonly_recursive(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            set_readonly_recursive(&entry.path())?;
        }
    }

    // Make files read-only but keep directories traversable
    if path.is_file() {
        let mut perms = std::fs::metadata(path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // r-xr-xr-x for executables, r--r--r-- for others
            let mode = perms.mode();
            if mode & 0o111 != 0 {
                perms.set_mode(0o555); // read + execute
            } else {
                perms.set_mode(0o444); // read only
            }
        }
        std::fs::set_permissions(path, perms)?;
    }

    Ok(())
}

/// Restore write permissions to a release directory (for cleanup).
pub fn release_sandbox(release_dir: &Path) -> std::io::Result<()> {
    set_writable_recursive(release_dir)
}

fn set_writable_recursive(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        // Make directory writable first so we can traverse
        let mut perms = std::fs::metadata(path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(path, perms)?;

        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            set_writable_recursive(&entry.path())?;
        }
    }

    if path.is_file() {
        let mut perms = std::fs::metadata(path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(path, perms)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_makes_release_readonly() {
        let tmp = tempfile::tempdir().unwrap();
        let release_dir = tmp.path().join("releases").join("001");
        let data_dir = tmp.path().join("data");

        std::fs::create_dir_all(&release_dir).unwrap();
        std::fs::write(release_dir.join("binary"), "#!/bin/sh\necho hi").unwrap();

        // Make it executable first
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                release_dir.join("binary"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        prepare_sandbox("test-app", &release_dir, &data_dir).unwrap();

        // Release files should be read-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(release_dir.join("binary"))
                .unwrap()
                .permissions();
            assert_eq!(perms.mode() & 0o777, 0o555);
        }

        // Data dir should exist and be writable
        assert!(data_dir.exists());

        // Restore for cleanup
        release_sandbox(&release_dir).unwrap();
    }
}
