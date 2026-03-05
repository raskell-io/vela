use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::{Child, Command};

use crate::config::{AppType, DeployStrategy};

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to start app '{app}': {reason}")]
    StartFailed { app: String, reason: String },
    #[error("app '{0}' is not running")]
    NotRunning(String),
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
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            running: HashMap::new(),
            port_range: 10000..=20000,
            used_ports: std::collections::HashSet::new(),
        }
    }

    fn allocate_port(&mut self) -> Result<u16, ProcessError> {
        for port in self.port_range.clone() {
            if !self.used_ports.contains(&port) {
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Set user-defined env vars
        for (key, value) in env_vars {
            // Substitute ${data_dir} in values
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

        // Store as pending (new instance), don't replace the running one yet
        // The caller handles the swap after health check
        self.running
            .insert(format!("{}:pending", app_name), process);

        Ok(port)
    }

    /// Promote pending instance to active and stop the old one.
    pub async fn swap(&mut self, app_name: &str, drain_seconds: u32) -> Result<(), ProcessError> {
        let pending_key = format!("{app_name}:pending");
        let active_key = app_name.to_string();

        // Stop old instance
        if let Some(mut old) = self.running.remove(&active_key) {
            tracing::info!(app = app_name, port = old.port, "draining old instance");
            tokio::time::sleep(std::time::Duration::from_secs(drain_seconds.into())).await;

            let _ = old.child.kill().await;
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

    /// Stop an app entirely.
    pub async fn stop(&mut self, app_name: &str) -> Result<(), ProcessError> {
        if let Some(mut process) = self.running.remove(app_name) {
            let _ = process.child.kill().await;
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

    /// List all active apps.
    pub fn list_active(&self) -> Vec<(&str, u16)> {
        self.running
            .iter()
            .filter(|(k, _)| !k.contains(":pending"))
            .map(|(_, p)| (p.app_name.as_str(), p.port))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_allocation() {
        let mut pm = ProcessManager::new();
        let port1 = pm.allocate_port().unwrap();
        let port2 = pm.allocate_port().unwrap();
        assert_ne!(port1, port2);
        assert!(port1 >= 10000);

        pm.release_port(port1);
        let port3 = pm.allocate_port().unwrap();
        assert_eq!(port3, port1); // reused
    }
}
