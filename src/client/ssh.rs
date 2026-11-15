use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create a tarball from an artifact (file or directory).
pub fn create_tarball(artifact: &Path, app_name: &str) -> Result<PathBuf> {
    let tarball = std::env::temp_dir().join(format!("vela-{app_name}.tar.gz"));

    let status = if artifact.is_dir() {
        Command::new("tar")
            .args([
                "czf",
                &tarball.to_string_lossy(),
                "-C",
                &artifact.to_string_lossy(),
                ".",
            ])
            .status()
            .context("failed to run tar")?
    } else {
        // Single file — wrap it in a tarball
        let parent = artifact.parent().unwrap_or(Path::new("."));
        let filename = artifact
            .file_name()
            .context("artifact has no filename")?
            .to_string_lossy();

        Command::new("tar")
            .args([
                "czf",
                &tarball.to_string_lossy(),
                "-C",
                &parent.to_string_lossy(),
                &filename,
            ])
            .status()
            .context("failed to run tar")?
    };

    if !status.success() {
        anyhow::bail!("tar failed with status {status}");
    }

    Ok(tarball)
}

/// Upload a tarball to the server via scp.
pub fn upload(server: &str, tarball: &Path, app_name: &str) -> Result<()> {
    let remote_path = format!("/tmp/vela-deploy-{app_name}.tar.gz");

    let status = Command::new("scp")
        .args([
            "-o",
            "StrictHostKeyChecking=accept-new",
            &tarball.to_string_lossy(),
            &format!("{server}:{remote_path}"),
        ])
        .status()
        .context("failed to run scp")?;

    if !status.success() {
        anyhow::bail!("scp failed with status {status}");
    }

    Ok(())
}

/// Tell the remote server to activate a deploy.
pub fn activate(server: &str, app_name: &str, manifest_toml: &str) -> Result<()> {
    // Send the manifest content via stdin to avoid escaping issues.
    let mut child = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=accept-new", server])
        .args(vela_command(&["_deploy", app_name]))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run ssh")?;

    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(manifest_toml.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("remote deploy activation failed");
    }

    Ok(())
}

/// Prefix for every `vela` invocation on the server. App state, secrets,
/// logs and the daemon's IPC socket are root-owned, so all remote commands
/// use the same scoped `sudo vela` path (a deploy user's sudoers entry must
/// allow `vela`).
const REMOTE_VELA: [&str; 2] = ["sudo", "vela"];

fn vela_command<'a>(args: &[&'a str]) -> Vec<&'a str> {
    REMOTE_VELA
        .iter()
        .copied()
        .chain(args.iter().copied())
        .collect()
}

/// Run a `vela` subcommand on the server and print its output.
pub fn run_vela(server: &str, args: &[&str]) -> Result<()> {
    run_remote(server, &vela_command(args))
}

/// Run a `vela` subcommand on the server with a TTY (for following logs).
pub fn run_vela_interactive(server: &str, args: &[&str]) -> Result<()> {
    run_remote_interactive(server, &vela_command(args))
}

/// Run a command on the remote server and print output.
fn run_remote(server: &str, args: &[&str]) -> Result<()> {
    let status = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=accept-new", server])
        .args(args)
        .status()
        .context("failed to run ssh")?;

    if !status.success() {
        anyhow::bail!("remote command failed with status {status}");
    }

    Ok(())
}

/// Upload source code to the server via git archive + scp.
/// Placed at the deploy tarball path so `_deploy` can find it.
pub fn upload_source(server: &str, app_name: &str) -> Result<()> {
    let tarball = std::env::temp_dir().join(format!("vela-source-{app_name}.tar.gz"));

    // Create a tarball of the current git repo
    let status = Command::new("git")
        .args([
            "archive",
            "--format=tar.gz",
            "-o",
            &tarball.to_string_lossy(),
            "HEAD",
        ])
        .status()
        .context("failed to run git archive — is this a git repo?")?;

    if !status.success() {
        anyhow::bail!("git archive failed");
    }

    // Upload to server at the deploy tarball path (_deploy looks here)
    let remote_path = format!("/tmp/vela-source-{app_name}.tar.gz");
    let status = Command::new("scp")
        .args([
            "-o",
            "StrictHostKeyChecking=accept-new",
            &tarball.to_string_lossy(),
            &format!("{server}:{remote_path}"),
        ])
        .status()
        .context("failed to upload source")?;

    if !status.success() {
        anyhow::bail!("scp failed uploading source");
    }

    let _ = std::fs::remove_file(&tarball);
    Ok(())
}

/// Run a command on the remote server interactively (for log tailing, etc).
fn run_remote_interactive(server: &str, args: &[&str]) -> Result<()> {
    let status = Command::new("ssh")
        .args(["-t", "-o", "StrictHostKeyChecking=accept-new", server])
        .args(args)
        .status()
        .context("failed to run ssh")?;

    if !status.success() {
        anyhow::bail!("remote command failed with status {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_vela_commands_use_sudo() {
        assert_eq!(
            vela_command(&["_rollback", "myapp"]),
            ["sudo", "vela", "_rollback", "myapp"]
        );
    }

    #[test]
    fn create_tarball_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("myapp");
        std::fs::write(&file, b"#!/bin/sh\necho hello").unwrap();

        let tarball = create_tarball(&file, "test-file-app").unwrap();
        assert!(tarball.exists());
        assert!(tarball.to_string_lossy().contains("vela-test-file-app"));

        std::fs::remove_file(&tarball).unwrap();
    }

    #[test]
    fn create_tarball_from_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("release");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("myapp"), b"binary").unwrap();
        std::fs::write(dir.join("config.toml"), b"[app]").unwrap();

        let tarball = create_tarball(&dir, "test-dir-app").unwrap();
        assert!(tarball.exists());

        std::fs::remove_file(&tarball).unwrap();
    }
}
