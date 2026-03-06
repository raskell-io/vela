use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Client-side manifest (Vela.toml in your project)
#[derive(Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub app: AppConfig,
    #[serde(default)]
    pub deploy: DeployConfig,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub resources: ResourceConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AppConfig {
    pub name: String,
    pub domain: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DeployConfig {
    /// SSH target: user@host
    #[serde(default)]
    pub server: Option<String>,

    /// "binary" or "beam"
    #[serde(default = "default_app_type")]
    pub r#type: AppType,

    /// Entrypoint binary name within the release directory
    #[serde(default)]
    pub binary: Option<String>,

    /// Health check endpoint path (e.g. "/health")
    #[serde(default)]
    pub health: Option<String>,

    /// Seconds to drain old connections before killing
    #[serde(default = "default_drain")]
    pub drain: u32,

    /// Deploy strategy
    #[serde(default)]
    pub strategy: DeployStrategy,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            server: None,
            r#type: AppType::Binary,
            binary: None,
            health: None,
            drain: default_drain(),
            strategy: DeployStrategy::BlueGreen,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ResourceConfig {
    /// Memory limit (e.g. "512M", "1G")
    #[serde(default)]
    pub memory: Option<String>,

    /// CPU weight (relative, default 100)
    #[serde(default)]
    pub cpu_weight: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AppType {
    #[default]
    Binary,
    Beam,
}

impl AppType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppType::Binary => "binary",
            AppType::Beam => "beam",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s {
            "beam" => AppType::Beam,
            _ => AppType::Binary,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DeployStrategy {
    #[default]
    BlueGreen,
    Sequential,
}

impl DeployStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStrategy::BlueGreen => "blue-green",
            DeployStrategy::Sequential => "sequential",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s {
            "sequential" => DeployStrategy::Sequential,
            _ => DeployStrategy::BlueGreen,
        }
    }
}

/// Server-side config (/etc/vela/server.toml)
#[derive(Debug, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default)]
    pub proxy: ProxyConfig,

    #[serde(default)]
    pub tls: TlsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            proxy: ProxyConfig::default(),
            tls: TlsConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ProxyConfig {
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    #[serde(default = "default_https_port")]
    pub https_port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            http_port: default_http_port(),
            https_port: default_https_port(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// ACME email for Let's Encrypt
    #[serde(default)]
    pub acme_email: Option<String>,

    /// Use Let's Encrypt staging (for testing)
    #[serde(default)]
    pub staging: bool,

    /// Path to TLS certificate (for Cloudflare Origin Certs or custom certs)
    #[serde(default)]
    pub cert: Option<PathBuf>,

    /// Path to TLS private key
    #[serde(default)]
    pub key: Option<PathBuf>,
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
        let manifest: Self = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {}", path.display(), e))?;
        Ok(manifest)
    }

    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let manifest: Self =
            toml::from_str(s).map_err(|e| anyhow::anyhow!("failed to parse manifest: {}", e))?;
        Ok(manifest)
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::info!("no server config at {}, using defaults", path.display());
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {}", path.display(), e))?;
        Ok(config)
    }

    pub fn apps_dir(&self) -> PathBuf {
        self.data_dir.join("apps")
    }

    pub fn secrets_dir(&self) -> PathBuf {
        self.data_dir.join("secrets")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("vela.sock")
    }
}

fn default_app_type() -> AppType {
    AppType::Binary
}

fn default_drain() -> u32 {
    5
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/vela")
}

fn default_http_port() -> u16 {
    80
}

fn default_https_port() -> u16 {
    443
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let toml_str = r#"
[app]
name = "cyanea"
domain = "cyanea.bio"
"#;
        let manifest: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.app.name, "cyanea");
        assert_eq!(manifest.app.domain, "cyanea.bio");
        assert_eq!(manifest.deploy.r#type, AppType::Binary);
        assert_eq!(manifest.deploy.strategy, DeployStrategy::BlueGreen);
    }

    #[test]
    fn parse_full_manifest() {
        let toml_str = r#"
[app]
name = "archipelag"
domain = "archipelag.io"

[deploy]
server = "root@hetzner.example.com"
type = "beam"
binary = "bin/server"
health = "/health"
drain = 10
strategy = "sequential"

[env]
DATABASE_PATH = "${data_dir}/archipelag.db"
SECRET_KEY_BASE = "${secret:SECRET_KEY_BASE}"

[resources]
memory = "1G"
cpu_weight = 200
"#;
        let manifest: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.app.name, "archipelag");
        assert_eq!(manifest.deploy.r#type, AppType::Beam);
        assert_eq!(manifest.deploy.strategy, DeployStrategy::Sequential);
        assert_eq!(manifest.deploy.drain, 10);
        assert_eq!(manifest.resources.memory.as_deref(), Some("1G"));
        assert_eq!(
            manifest.env.get("DATABASE_PATH").map(String::as_str),
            Some("${data_dir}/archipelag.db")
        );
    }

    #[test]
    fn parse_from_toml_str() {
        let s = r#"
[app]
name = "test"
domain = "test.com"
"#;
        let manifest = Manifest::from_toml_str(s).unwrap();
        assert_eq!(manifest.app.name, "test");
    }

    #[test]
    fn parse_server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.data_dir, PathBuf::from("/var/vela"));
        assert_eq!(config.proxy.http_port, 80);
        assert_eq!(config.proxy.https_port, 443);
    }

    #[test]
    fn parse_server_config() {
        let toml_str = r#"
data_dir = "/opt/vela"

[proxy]
http_port = 8080
https_port = 8443

[tls]
acme_email = "ops@example.com"
staging = true
"#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/opt/vela"));
        assert_eq!(config.proxy.http_port, 8080);
        assert_eq!(config.tls.acme_email.as_deref(), Some("ops@example.com"));
        assert!(config.tls.staging);
    }

    #[test]
    fn parse_server_config_with_static_tls() {
        let toml_str = r#"
[tls]
cert = "/etc/vela/tls/origin.pem"
key = "/etc/vela/tls/origin-key.pem"
"#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.tls.cert.as_deref(),
            Some(Path::new("/etc/vela/tls/origin.pem"))
        );
        assert_eq!(
            config.tls.key.as_deref(),
            Some(Path::new("/etc/vela/tls/origin-key.pem"))
        );
    }

    #[test]
    fn app_type_round_trip() {
        assert_eq!(AppType::Binary.as_str(), "binary");
        assert_eq!(AppType::Beam.as_str(), "beam");
        assert_eq!(AppType::from_str_loose("beam"), AppType::Beam);
        assert_eq!(AppType::from_str_loose("binary"), AppType::Binary);
        assert_eq!(AppType::from_str_loose("unknown"), AppType::Binary);
    }

    #[test]
    fn deploy_strategy_round_trip() {
        assert_eq!(DeployStrategy::BlueGreen.as_str(), "blue-green");
        assert_eq!(DeployStrategy::Sequential.as_str(), "sequential");
        assert_eq!(
            DeployStrategy::from_str_loose("sequential"),
            DeployStrategy::Sequential
        );
        assert_eq!(
            DeployStrategy::from_str_loose("blue-green"),
            DeployStrategy::BlueGreen
        );
    }
}
