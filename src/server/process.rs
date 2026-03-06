use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::process::{Child, Command};

use crate::config::AppType;

const DEFAULT_DRAIN_SECONDS: u32 = 10;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to start app '{app}': {reason}")]
    StartFailed { app: String, reason: String },
    #[error("app '{0}' is not running")]
    NotRunning(String),
    #[error("deploy already in progress for '{0}'")]
    DeployInProgress(String),
    #[error("no port available")]
    NoPortAvailable,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct AppProcess {
    pub app_name: String,
    pub release_id: String,
    pub port: u16,
    pub child: Child,
}

pub struct ProcessManager {
    running: HashMap<String, AppProcess>,
    port_range: std::ops::RangeInclusive<u16>,
    used_ports: std::collections::HashSet<u16>,
    logs_dir: PathBuf,
}

impl ProcessManager {
    pub fn new(logs_dir: PathBuf) -> Self {
        Self {
            running: HashMap::new(),
            port_range: 10000..=20000,
            used_ports: std::collections::HashSet::new(),
            logs_dir,
        }
    }

    fn allocate_port(&mut self) -> Result<u16, ProcessError> {
        for port in self.port_range.clone() {
            if !self.used_ports.contains(&port) && is_port_available(port) {
                self.used_ports.insert(port);
                return Ok(port);
            }
        }
        Err(ProcessError::NoPortAvailable)
    }

    fn release_port(&mut self, port: u16) {
        self.used_ports.remove(&port);
    }

    /// Start a new instance of an app. Returns the port it's listening on.
    pub async fn start(
        &mut self,
        app_name: &str,
        release_dir: &Path,
        binary_name: &str,
        app_type: AppType,
        data_dir: &Path,
        env_vars: &[(String, String)],
    ) -> Result<u16, ProcessError> {
        // Reject if there's already a pending deploy for this app
        let pending_key = format!("{app_name}:pending");
        if self.running.contains_key(&pending_key) {
            return Err(ProcessError::DeployInProgress(app_name.to_string()));
        }

        let port = self.allocate_port()?;

        let entrypoint = match app_type {
            AppType::Binary => release_dir.join(binary_name),
            AppType::Beam => release_dir.join(binary_name),
        };

        if !entrypoint.exists() {
            return Err(ProcessError::StartFailed {
                app: app_name.to_string(),
                reason: format!("entrypoint not found: {}", entrypoint.display()),
            });
        }

        // Set up log files (append mode — don't truncate existing logs)
        let app_log_dir = self.logs_dir.join(app_name);
        std::fs::create_dir_all(&app_log_dir)?;

        let stdout_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(app_log_dir.join("stdout.log"))
            .map_err(|e| ProcessError::StartFailed {
                app: app_name.to_string(),
                reason: format!("failed to open stdout log: {e}"),
            })?;
        let stderr_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(app_log_dir.join("stderr.log"))
            .map_err(|e| ProcessError::StartFailed {
                app: app_name.to_string(),
                reason: format!("failed to open stderr log: {e}"),
            })?;

        let mut cmd = match app_type {
            AppType::Binary => Command::new(&entrypoint),
            AppType::Beam => {
                let mut c = Command::new(&entrypoint);
                c.arg("start");
                c
            }
        };

        cmd.env("PORT", port.to_string())
            .env("VELA_PORT", port.to_string())
            .env("VELA_DATA_DIR", data_dir)
            .env("VELA_APP_NAME", app_name)
            .stdin(std::process::Stdio::null())
            .stdout(stdout_file)
            .stderr(stderr_file);

        // Set user-defined env vars
        for (key, value) in env_vars {
            let resolved = value.replace("${data_dir}", &data_dir.to_string_lossy());
            cmd.env(key, resolved);
        }

        let child = cmd.spawn().map_err(|e| ProcessError::StartFailed {
            app: app_name.to_string(),
            reason: e.to_string(),
        })?;

        tracing::info!(
            app = app_name,
            port,
            entrypoint = %entrypoint.display(),
            "started app process"
        );

        let process = AppProcess {
            app_name: app_name.to_string(),
            release_id: release_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            port,
            child,
        };

        self.running.insert(pending_key, process);

        Ok(port)
    }

    /// Promote pending instance to active, gracefully stopping the old one.
    pub async fn swap(&mut self, app_name: &str, drain_seconds: u32) -> Result<(), ProcessError> {
        let pending_key = format!("{app_name}:pending");
        let active_key = app_name.to_string();

        // Gracefully stop old instance
        if let Some(mut old) = self.running.remove(&active_key) {
            tracing::info!(app = app_name, port = old.port, "draining old instance");
            graceful_stop(&mut old.child, drain_seconds).await;
            self.release_port(old.port);
            tracing::info!(app = app_name, "old instance stopped");
        }

        // Move pending to active
        if let Some(pending) = self.running.remove(&pending_key) {
            tracing::info!(
                app = app_name,
                port = pending.port,
                release = %pending.release_id,
                "activated new instance"
            );
            self.running.insert(active_key, pending);
        }

        Ok(())
    }

    /// Stop a pending instance (deploy failed).
    pub async fn abort_pending(&mut self, app_name: &str) -> Result<(), ProcessError> {
        let pending_key = format!("{app_name}:pending");
        if let Some(mut pending) = self.running.remove(&pending_key) {
            let _ = pending.child.kill().await;
            self.release_port(pending.port);
            tracing::warn!(app = app_name, "aborted pending deploy");
        }
        Ok(())
    }

    /// Stop an app entirely with graceful shutdown.
    pub async fn stop(&mut self, app_name: &str) -> Result<(), ProcessError> {
        if let Some(mut process) = self.running.remove(app_name) {
            graceful_stop(&mut process.child, DEFAULT_DRAIN_SECONDS).await;
            self.release_port(process.port);
            tracing::info!(app = app_name, "stopped");
            Ok(())
        } else {
            Err(ProcessError::NotRunning(app_name.to_string()))
        }
    }

    /// Get the port for a currently active app.
    pub fn active_port(&self, app_name: &str) -> Option<u16> {
        self.running.get(app_name).map(|p| p.port)
    }

    /// Get the port for a pending app (during deploy).
    pub fn pending_port(&self, app_name: &str) -> Option<u16> {
        self.running
            .get(&format!("{app_name}:pending"))
            .map(|p| p.port)
    }

    /// Promote a pending instance directly to active (without swap/drain).
    /// Used during restore when there's no old instance to drain.
    pub fn promote_pending_to_active(&mut self, app_name: &str) {
        let pending_key = format!("{app_name}:pending");
        if let Some(process) = self.running.remove(&pending_key) {
            self.running.insert(app_name.to_string(), process);
        }
    }

    /// List all active apps.
    pub fn list_active(&self) -> Vec<(&str, u16)> {
        self.running
            .iter()
            .filter(|(k, _)| !k.contains(":pending"))
            .map(|(_, p)| (p.app_name.as_str(), p.port))
            .collect()
    }
}

/// Check if a port is available on the system.
fn is_port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Send SIGTERM, wait for graceful exit, then SIGKILL if needed.
async fn graceful_stop(child: &mut Child, timeout_seconds: u32) {
    // Send SIGTERM
    if let Some(pid) = child.id() {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        tracing::debug!(pid, "sent SIGTERM");
    }

    // Wait for process to exit gracefully
    match tokio::time::timeout(Duration::from_secs(timeout_seconds.into()), child.wait()).await {
        Ok(Ok(status)) => {
            tracing::debug!(?status, "process exited gracefully");
        }
        Ok(Err(e)) => {
            tracing::debug!(err = %e, "error waiting for process");
        }
        Err(_) => {
            tracing::warn!(
                timeout = timeout_seconds,
                "process did not exit in time, sending SIGKILL"
            );
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_allocation() {
        let mut pm = ProcessManager::new(std::path::PathBuf::from("/tmp/vela-test-logs"));
        let port1 = pm.allocate_port().unwrap();
        let port2 = pm.allocate_port().unwrap();
        assert_ne!(port1, port2);
        assert!(port1 >= 10000);

        pm.release_port(port1);
        let port3 = pm.allocate_port().unwrap();
        assert_eq!(port3, port1); // reused
    }

    #[test]
    fn port_availability_check() {
        // Bind a port, then check it's not available
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_port_available(port));
        drop(listener);
        assert!(is_port_available(port));
    }
}
