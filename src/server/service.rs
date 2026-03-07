//! Service dependency management.
//!
//! Manages external services (Postgres, NATS, etc.) that apps depend on.
//! Services are provisioned on first use and supervised by the daemon.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Child;

// ---------------------------------------------------------------------------
// Service-specific config types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostgresConfig {
    #[serde(default = "default_pg_version")]
    pub version: String,
    #[serde(default)]
    pub databases: Vec<String>,
}

fn default_pg_version() -> String {
    "17".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NatsConfig {
    #[serde(default = "default_nats_version")]
    pub version: String,
    #[serde(default)]
    pub jetstream: bool,
}

fn default_nats_version() -> String {
    "2.10".to_string()
}

// ---------------------------------------------------------------------------
// Service state (persisted to disk)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub service_type: String,
    pub provisioned: bool,
    /// Per-database credentials for Postgres.
    #[serde(default)]
    pub credentials: HashMap<String, String>,
    /// Port the service listens on (for NATS).
    pub port: Option<u16>,
}

// ---------------------------------------------------------------------------
// ServiceManager
// ---------------------------------------------------------------------------

pub struct ServiceManager {
    services_dir: PathBuf,
    /// Running NATS process (owned by daemon). Postgres is managed by systemd.
    nats_process: Option<Child>,
}

impl ServiceManager {
    pub fn new(data_dir: &Path) -> Self {
        let services_dir = data_dir.join("services");
        std::fs::create_dir_all(&services_dir).ok();
        Self {
            services_dir,
            nats_process: None,
        }
    }

    /// Ensure a service is provisioned and running.
    /// Returns environment variables to inject into apps.
    pub async fn ensure_service(
        &mut self,
        service_type: &str,
        config: &toml::Value,
    ) -> Result<Vec<(String, String)>> {
        match service_type {
            "postgres" => {
                let pg_config: PostgresConfig = config
                    .clone()
                    .try_into()
                    .context("invalid [services.postgres] config")?;
                self.ensure_postgres(&pg_config).await
            }
            "nats" => {
                let nats_config: NatsConfig = config
                    .clone()
                    .try_into()
                    .context("invalid [services.nats] config")?;
                self.ensure_nats(&nats_config).await
            }
            other => anyhow::bail!("unknown service type: {other}"),
        }
    }

    /// Restore services from persisted state (called on daemon startup).
    pub async fn restore(&mut self) -> Result<()> {
        // Restore NATS if it was previously running
        let nats_state_path = self.services_dir.join("nats").join("state.toml");
        if nats_state_path.exists() {
            let state = load_service_state(&nats_state_path)?;
            if state.provisioned {
                tracing::info!("restoring NATS service");
                let nats_dir = self.services_dir.join("nats");
                if let Err(e) = self.start_nats_process(&nats_dir).await {
                    tracing::error!(err = %e, "failed to restore NATS");
                }
            }
        }

        // Postgres is managed by systemd — just verify it's running
        let pg_state_path = self.services_dir.join("postgres").join("state.toml");
        if pg_state_path.exists() {
            if !pg_is_ready() {
                tracing::warn!("postgres was previously provisioned but is not running");
                let _ = run_cmd("systemctl", &["start", "postgresql"]);
            }
        }

        Ok(())
    }

    /// Check NATS process and restart if crashed.
    pub async fn check_and_restart(&mut self) {
        if let Some(ref mut child) = self.nats_process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!(?status, "NATS process exited, restarting");
                    let nats_dir = self.services_dir.join("nats");
                    self.nats_process = None;
                    if let Err(e) = self.start_nats_process(&nats_dir).await {
                        tracing::error!(err = %e, "failed to restart NATS");
                    }
                }
                Ok(None) => {} // still running
                Err(e) => {
                    tracing::warn!(err = %e, "failed to check NATS process status");
                }
            }
        }
    }

    /// Stop all managed services.
    pub async fn stop_all(&mut self) {
        if let Some(ref mut child) = self.nats_process {
            tracing::info!("stopping NATS");
            let _ = child.kill().await;
            self.nats_process = None;
        }
    }

    /// Get environment variables for all services an app depends on.
    pub fn env_vars_for_services(
        &self,
        services: &HashMap<String, toml::Value>,
    ) -> Result<Vec<(String, String)>> {
        let mut vars = Vec::new();

        for (svc_type, config) in services {
            match svc_type.as_str() {
                "postgres" => {
                    let pg_config: PostgresConfig = config
                        .clone()
                        .try_into()
                        .context("invalid [services.postgres] config")?;
                    let state = self.load_state("postgres")?;
                    if let Some(state) = state {
                        // Inject DATABASE_URL for each database
                        for db_name in &pg_config.databases {
                            let password = state.credentials.get(db_name).cloned().unwrap_or_default();
                            let user = db_name;
                            let url = format!("postgres://{user}:{password}@localhost/{db_name}");
                            vars.push(("DATABASE_URL".to_string(), url));
                        }
                    }
                }
                "nats" => {
                    let port = self
                        .load_state("nats")?
                        .and_then(|s| s.port)
                        .unwrap_or(4222);
                    vars.push(("NATS_URL".to_string(), format!("nats://localhost:{port}")));
                }
                _ => {}
            }
        }

        Ok(vars)
    }

    // -----------------------------------------------------------------------
    // Postgres
    // -----------------------------------------------------------------------

    async fn ensure_postgres(&mut self, config: &PostgresConfig) -> Result<Vec<(String, String)>> {
        let pg_dir = self.services_dir.join("postgres");
        std::fs::create_dir_all(&pg_dir)?;

        // Check if postgres is installed
        if !command_exists("pg_isready") {
            tracing::info!(version = %config.version, "installing PostgreSQL");
            install_postgres(&config.version)?;
        }

        // Ensure running
        if !pg_is_ready() {
            tracing::info!("starting PostgreSQL");
            run_cmd("systemctl", &["start", "postgresql"])?;
            // Wait for readiness
            for _ in 0..30 {
                if pg_is_ready() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            if !pg_is_ready() {
                anyhow::bail!("PostgreSQL failed to start");
            }
        }

        // Load or create state
        let state_path = pg_dir.join("state.toml");
        let mut state = load_service_state(&state_path).unwrap_or_else(|_| ServiceState {
            service_type: "postgres".into(),
            provisioned: false,
            credentials: HashMap::new(),
            port: Some(5432),
        });

        // Provision databases
        let mut env_vars = Vec::new();
        for db_name in &config.databases {
            if !state.credentials.contains_key(db_name) {
                let password = generate_password()?;
                provision_postgres_db(db_name, &password)?;
                state.credentials.insert(db_name.clone(), password.clone());
                tracing::info!(database = db_name, "provisioned Postgres database");
            }

            let password = state.credentials.get(db_name).unwrap();
            let url = format!("postgres://{db_name}:{password}@localhost/{db_name}");
            env_vars.push(("DATABASE_URL".to_string(), url));
        }

        state.provisioned = true;
        save_service_state(&state_path, &state)?;

        Ok(env_vars)
    }

    // -----------------------------------------------------------------------
    // NATS
    // -----------------------------------------------------------------------

    async fn ensure_nats(&mut self, config: &NatsConfig) -> Result<Vec<(String, String)>> {
        let nats_dir = self.services_dir.join("nats");
        std::fs::create_dir_all(&nats_dir)?;

        let binary = nats_dir.join("nats-server");

        // Download if not present
        if !binary.exists() {
            tracing::info!(version = %config.version, "downloading NATS server");
            download_nats(&config.version, &nats_dir)?;
        }

        // Generate config
        let conf_path = nats_dir.join("nats.conf");
        let data_dir = nats_dir.join("data");
        std::fs::create_dir_all(&data_dir)?;
        write_nats_config(&conf_path, config, &data_dir)?;

        // Start if not running
        if self.nats_process.is_none() {
            self.start_nats_process(&nats_dir).await?;
        }

        // Save state
        let state = ServiceState {
            service_type: "nats".into(),
            provisioned: true,
            credentials: HashMap::new(),
            port: Some(4222),
        };
        save_service_state(&nats_dir.join("state.toml"), &state)?;

        Ok(vec![(
            "NATS_URL".to_string(),
            "nats://localhost:4222".to_string(),
        )])
    }

    async fn start_nats_process(&mut self, nats_dir: &Path) -> Result<()> {
        let binary = nats_dir.join("nats-server");
        let conf = nats_dir.join("nats.conf");
        let log_file = nats_dir.join("nats.log");

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)?;

        let child = tokio::process::Command::new(&binary)
            .args(["-c", &conf.to_string_lossy()])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("failed to start NATS server")?;

        tracing::info!("NATS server started");

        // Wait for readiness
        for _ in 0..30 {
            if nats_is_ready().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        self.nats_process = Some(child);
        Ok(())
    }

    fn load_state(&self, service_type: &str) -> Result<Option<ServiceState>> {
        let path = self.services_dir.join(service_type).join("state.toml");
        if !path.exists() {
            return Ok(None);
        }
        let state = load_service_state(&path)?;
        Ok(Some(state))
    }
}

// ---------------------------------------------------------------------------
// Postgres helpers
// ---------------------------------------------------------------------------

fn pg_is_ready() -> bool {
    std::process::Command::new("pg_isready")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn install_postgres(version: &str) -> Result<()> {
    run_cmd("apt-get", &["update", "-qq"])?;
    let pkg = format!("postgresql-{version}");
    run_cmd("apt-get", &["install", "-y", "-qq", &pkg])?;
    run_cmd("systemctl", &["enable", "postgresql"])?;
    run_cmd("systemctl", &["start", "postgresql"])?;
    Ok(())
}

fn provision_postgres_db(db_name: &str, password: &str) -> Result<()> {
    // Create user (ignore error if exists)
    let create_user = format!(
        "CREATE USER \"{db_name}\" WITH PASSWORD '{password}';",
    );
    let _ = run_cmd_as_postgres(&create_user);

    // Create database
    let create_db = format!(
        "CREATE DATABASE \"{db_name}\" OWNER \"{db_name}\";",
    );
    let _ = run_cmd_as_postgres(&create_db);

    // Grant privileges
    let grant = format!(
        "GRANT ALL PRIVILEGES ON DATABASE \"{db_name}\" TO \"{db_name}\";",
    );
    let _ = run_cmd_as_postgres(&grant);

    Ok(())
}

fn run_cmd_as_postgres(sql: &str) -> Result<()> {
    let status = std::process::Command::new("su")
        .args(["-", "postgres", "-c", &format!("psql -c \"{sql}\"")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to run psql")?;
    if !status.success() {
        anyhow::bail!("psql command failed: {sql}");
    }
    Ok(())
}

/// Back up Postgres databases via pg_dump.
pub fn backup_postgres_databases(
    services_dir: &Path,
    dest_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let state_path = services_dir.join("postgres").join("state.toml");
    if !state_path.exists() {
        return Ok(Vec::new());
    }

    let state = load_service_state(&state_path)?;
    let mut paths = Vec::new();

    for (db_name, _password) in &state.credentials {
        let dump_path = dest_dir.join(format!("{db_name}.sql.gz"));
        let dump_cmd = format!(
            "pg_dump -U {db_name} {db_name} | gzip > {}",
            dump_path.display()
        );
        let status = std::process::Command::new("su")
            .args(["-", "postgres", "-c", &dump_cmd])
            .status()
            .with_context(|| format!("failed to dump database {db_name}"))?;

        if status.success() {
            tracing::info!(database = db_name, "backed up Postgres database");
            paths.push(dump_path);
        } else {
            tracing::warn!(database = db_name, "pg_dump failed");
        }
    }

    Ok(paths)
}

// ---------------------------------------------------------------------------
// NATS helpers
// ---------------------------------------------------------------------------

fn download_nats(version: &str, nats_dir: &Path) -> Result<()> {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };

    let tarball = format!("nats-server-v{version}-linux-{arch}.tar.gz");
    let url = format!(
        "https://github.com/nats-io/nats-server/releases/download/v{version}/{tarball}"
    );

    let download_path = nats_dir.join(&tarball);

    // Download
    let status = std::process::Command::new("curl")
        .args(["-fsSL", "-o", &download_path.to_string_lossy(), &url])
        .status()
        .context("failed to download NATS server")?;

    if !status.success() {
        anyhow::bail!("failed to download NATS from {url}");
    }

    // Extract
    let status = std::process::Command::new("tar")
        .args([
            "xzf",
            &download_path.to_string_lossy(),
            "-C",
            &nats_dir.to_string_lossy(),
            "--strip-components=1",
        ])
        .status()
        .context("failed to extract NATS tarball")?;

    if !status.success() {
        anyhow::bail!("failed to extract NATS tarball");
    }

    // Clean up tarball
    let _ = std::fs::remove_file(&download_path);

    // Verify binary exists
    let binary = nats_dir.join("nats-server");
    if !binary.exists() {
        anyhow::bail!("NATS binary not found after extraction");
    }

    Ok(())
}

fn write_nats_config(conf_path: &Path, config: &NatsConfig, data_dir: &Path) -> Result<()> {
    let mut conf = String::new();
    conf.push_str("# Managed by Vela — do not edit\n\n");
    conf.push_str("listen: 127.0.0.1:4222\n");
    conf.push_str("http: 127.0.0.1:8222\n\n");

    if config.jetstream {
        conf.push_str("jetstream {\n");
        conf.push_str(&format!("  store_dir: \"{}\"\n", data_dir.display()));
        conf.push_str("}\n");
    }

    std::fs::write(conf_path, conf)?;
    Ok(())
}

async fn nats_is_ready() -> bool {
    reqwest::Client::new()
        .get("http://127.0.0.1:8222/healthz")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn run_cmd(cmd: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {cmd}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd} failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn command_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn generate_password() -> Result<String> {
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn load_service_state(path: &Path) -> Result<ServiceState> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state: ServiceState = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(state)
}

fn save_service_state(path: &Path, state: &ServiceState) -> Result<()> {
    let content = toml::to_string_pretty(state)?;
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_postgres_config() {
        let value: toml::Value = toml::from_str(
            r#"
            version = "17"
            databases = ["myapp_prod", "myapp_test"]
            "#,
        )
        .unwrap();
        let config: PostgresConfig = value.try_into().unwrap();
        assert_eq!(config.version, "17");
        assert_eq!(config.databases, vec!["myapp_prod", "myapp_test"]);
    }

    #[test]
    fn parse_nats_config() {
        let value: toml::Value = toml::from_str(
            r#"
            version = "2.10"
            jetstream = true
            "#,
        )
        .unwrap();
        let config: NatsConfig = value.try_into().unwrap();
        assert_eq!(config.version, "2.10");
        assert!(config.jetstream);
    }

    #[test]
    fn nats_config_defaults() {
        let value: toml::Value = toml::from_str("").unwrap();
        let config: NatsConfig = value.try_into().unwrap();
        assert_eq!(config.version, "2.10");
        assert!(!config.jetstream);
    }

    #[test]
    fn service_state_roundtrip() {
        let state = ServiceState {
            service_type: "postgres".into(),
            provisioned: true,
            credentials: HashMap::from([("mydb".into(), "secret123".into())]),
            port: Some(5432),
        };
        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: ServiceState = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.service_type, "postgres");
        assert!(deserialized.provisioned);
        assert_eq!(deserialized.credentials.get("mydb").unwrap(), "secret123");
    }

    #[test]
    fn nats_config_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let conf_path = tmp.path().join("nats.conf");
        let data_dir = tmp.path().join("data");

        let config = NatsConfig {
            version: "2.10".into(),
            jetstream: true,
        };

        write_nats_config(&conf_path, &config, &data_dir).unwrap();

        let content = std::fs::read_to_string(&conf_path).unwrap();
        assert!(content.contains("listen: 127.0.0.1:4222"));
        assert!(content.contains("jetstream"));
        assert!(content.contains(&data_dir.to_string_lossy().to_string()));
    }

    #[test]
    fn nats_config_without_jetstream() {
        let tmp = tempfile::tempdir().unwrap();
        let conf_path = tmp.path().join("nats.conf");
        let data_dir = tmp.path().join("data");

        let config = NatsConfig {
            version: "2.10".into(),
            jetstream: false,
        };

        write_nats_config(&conf_path, &config, &data_dir).unwrap();

        let content = std::fs::read_to_string(&conf_path).unwrap();
        assert!(content.contains("listen: 127.0.0.1:4222"));
        assert!(!content.contains("jetstream"));
    }
}
