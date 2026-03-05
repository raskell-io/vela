use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

/// App configuration stored as app.toml inside each app directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub domain: String,
    pub app_type: String,
    pub binary_name: Option<String>,
    pub health_path: Option<String>,
    pub deploy_strategy: String,
    pub drain_seconds: u32,
}

/// Runtime info derived from the filesystem.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub domain: String,
    pub current_release: String,
    pub status: String,
}

/// Filesystem-backed server state.
///
/// All state is stored as plain files under `data_dir`:
/// ```text
/// /var/vela/apps/<name>/
/// ├── app.toml          # AppConfig
/// ├── secrets.env       # KEY=VALUE per line
/// ├── data/             # persistent app data
/// ├── releases/
/// │   ├── 20260305-001/
/// │   └── 20260305-002/
/// └── current -> releases/20260305-002
/// ```
pub struct ServerState {
    apps_dir: PathBuf,
}

impl ServerState {
    pub fn open(config: &ServerConfig) -> Result<Self> {
        let apps_dir = config.apps_dir();
        std::fs::create_dir_all(&apps_dir)?;
        Ok(Self { apps_dir })
    }

    fn app_dir(&self, name: &str) -> PathBuf {
        self.apps_dir.join(name)
    }

    fn app_config_path(&self, name: &str) -> PathBuf {
        self.app_dir(name).join("app.toml")
    }

    fn secrets_path(&self, name: &str) -> PathBuf {
        self.app_dir(name).join("secrets.env")
    }

    // -----------------------------------------------------------------------
    // Apps
    // -----------------------------------------------------------------------

    pub fn list_apps(&self) -> Result<Vec<AppInfo>> {
        let mut apps = Vec::new();

        let entries = match std::fs::read_dir(&self.apps_dir) {
            Ok(e) => e,
            Err(_) => return Ok(apps),
        };

        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let config_path = self.app_config_path(&name);

            if !config_path.exists() {
                continue;
            }

            let app_config = self.load_app_config(&name)?;
            let current_release = self.get_active_release(&name)?;
            let status = if current_release.is_some() {
                "active"
            } else {
                "no-release"
            };

            apps.push(AppInfo {
                name,
                domain: app_config.domain,
                current_release: current_release.unwrap_or_else(|| "none".into()),
                status: status.into(),
            });
        }

        apps.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(apps)
    }

    pub fn register_app(
        &self,
        name: &str,
        domain: &str,
        app_type: &str,
        binary_name: Option<&str>,
        health_path: Option<&str>,
        deploy_strategy: &str,
        drain_seconds: u32,
    ) -> Result<()> {
        let app_dir = self.app_dir(name);
        std::fs::create_dir_all(&app_dir)?;

        let config = AppConfig {
            name: name.into(),
            domain: domain.into(),
            app_type: app_type.into(),
            binary_name: binary_name.map(Into::into),
            health_path: health_path.map(Into::into),
            deploy_strategy: deploy_strategy.into(),
            drain_seconds,
        };

        let toml_str = toml::to_string_pretty(&config).context("failed to serialize app config")?;
        std::fs::write(self.app_config_path(name), toml_str)?;

        Ok(())
    }

    pub fn get_app(&self, name: &str) -> Result<Option<AppConfig>> {
        let path = self.app_config_path(name);
        if !path.exists() {
            return Ok(None);
        }
        let config = self.load_app_config(name)?;
        Ok(Some(config))
    }

    fn load_app_config(&self, name: &str) -> Result<AppConfig> {
        let path = self.app_config_path(name);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(config)
    }

    pub fn list_active_apps(&self) -> Result<Vec<AppConfig>> {
        let mut active = Vec::new();

        let entries = match std::fs::read_dir(&self.apps_dir) {
            Ok(e) => e,
            Err(_) => return Ok(active),
        };

        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let current = self.app_dir(&name).join("current");

            // Only restore apps that have a current symlink (i.e. have been deployed)
            if !current.is_symlink() {
                continue;
            }

            if let Ok(config) = self.load_app_config(&name) {
                active.push(config);
            }
        }

        Ok(active)
    }

    // -----------------------------------------------------------------------
    // Releases
    // -----------------------------------------------------------------------

    pub fn get_active_release(&self, app_name: &str) -> Result<Option<String>> {
        let current = self.app_dir(app_name).join("current");
        if !current.is_symlink() {
            return Ok(None);
        }

        let target = std::fs::read_link(&current)?;
        let release_id = target
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        if release_id.is_empty() {
            return Ok(None);
        }

        Ok(Some(release_id))
    }

    pub fn get_previous_release(&self, app_name: &str) -> Result<Option<String>> {
        let active = self.get_active_release(app_name)?;
        let releases = self.list_releases(app_name)?;

        match active {
            Some(current) => {
                // Find the release before the current one
                let pos = releases.iter().position(|r| r == &current);
                match pos {
                    Some(i) if i > 0 => Ok(Some(releases[i - 1].clone())),
                    _ => Ok(None),
                }
            }
            None => {
                // No active release — return the latest
                Ok(releases.last().cloned())
            }
        }
    }

    fn list_releases(&self, app_name: &str) -> Result<Vec<String>> {
        let releases_dir = self.app_dir(app_name).join("releases");
        if !releases_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries: Vec<String> = std::fs::read_dir(&releases_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        // Sort chronologically (timestamp-based names sort lexicographically)
        entries.sort();
        Ok(entries)
    }

    // -----------------------------------------------------------------------
    // Secrets
    // -----------------------------------------------------------------------

    pub fn set_secret(&self, app_name: &str, key: &str, value: &str) -> Result<()> {
        let mut secrets = self.load_secrets(app_name)?;
        secrets.insert(key.to_string(), value.to_string());
        self.save_secrets(app_name, &secrets)
    }

    pub fn get_secrets(&self, app_name: &str) -> Result<Vec<(String, String)>> {
        let secrets = self.load_secrets(app_name)?;
        let mut pairs: Vec<(String, String)> = secrets.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(pairs)
    }

    pub fn remove_secret(&self, app_name: &str, key: &str) -> Result<bool> {
        let mut secrets = self.load_secrets(app_name)?;
        let removed = secrets.remove(key).is_some();
        if removed {
            self.save_secrets(app_name, &secrets)?;
        }
        Ok(removed)
    }

    fn load_secrets(&self, app_name: &str) -> Result<HashMap<String, String>> {
        let path = self.secrets_path(app_name);
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let content = std::fs::read_to_string(&path)?;
        let mut secrets = HashMap::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                secrets.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        Ok(secrets)
    }

    fn save_secrets(&self, app_name: &str, secrets: &HashMap<String, String>) -> Result<()> {
        let path = self.secrets_path(app_name);

        // Ensure the app directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut lines: Vec<String> = secrets.iter().map(|(k, v)| format!("{k}={v}")).collect();
        lines.sort();

        let content = lines.join("\n") + "\n";
        std::fs::write(&path, content)?;

        // Restrict permissions (secrets file)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> (tempfile::TempDir, ServerState) {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let state = ServerState::open(&config).unwrap();
        (dir, state)
    }

    #[test]
    fn empty_apps() {
        let (_dir, state) = test_state();
        let apps = state.list_apps().unwrap();
        assert!(apps.is_empty());
    }

    #[test]
    fn register_and_list_app() {
        let (_dir, state) = test_state();

        state
            .register_app(
                "cyanea",
                "cyanea.bio",
                "binary",
                Some("cyanea-server"),
                Some("/health"),
                "blue-green",
                5,
            )
            .unwrap();

        let apps = state.list_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "cyanea");
        assert_eq!(apps[0].domain, "cyanea.bio");
        assert_eq!(apps[0].status, "no-release");
    }

    #[test]
    fn get_app() {
        let (_dir, state) = test_state();

        state
            .register_app(
                "myapp",
                "myapp.com",
                "beam",
                Some("bin/server"),
                None,
                "sequential",
                10,
            )
            .unwrap();

        let app = state.get_app("myapp").unwrap().unwrap();
        assert_eq!(app.app_type, "beam");
        assert_eq!(app.binary_name.as_deref(), Some("bin/server"));
        assert_eq!(app.deploy_strategy, "sequential");
        assert_eq!(app.drain_seconds, 10);

        assert!(state.get_app("nonexistent").unwrap().is_none());
    }

    #[test]
    fn active_release_from_symlink() {
        let (_dir, state) = test_state();

        state
            .register_app("myapp", "myapp.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        // No release yet
        assert!(state.get_active_release("myapp").unwrap().is_none());

        // Create a release and symlink
        let release_dir = state.app_dir("myapp").join("releases").join("20260305-001");
        std::fs::create_dir_all(&release_dir).unwrap();
        std::os::unix::fs::symlink(&release_dir, state.app_dir("myapp").join("current")).unwrap();

        assert_eq!(
            state.get_active_release("myapp").unwrap(),
            Some("20260305-001".into())
        );
    }

    #[test]
    fn previous_release() {
        let (_dir, state) = test_state();

        state
            .register_app("myapp", "myapp.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        let releases_dir = state.app_dir("myapp").join("releases");
        std::fs::create_dir_all(releases_dir.join("20260305-001")).unwrap();
        std::fs::create_dir_all(releases_dir.join("20260305-002")).unwrap();

        // Point current at the second release
        std::os::unix::fs::symlink(
            releases_dir.join("20260305-002"),
            state.app_dir("myapp").join("current"),
        )
        .unwrap();

        assert_eq!(
            state.get_previous_release("myapp").unwrap(),
            Some("20260305-001".into())
        );
    }

    #[test]
    fn secrets_crud() {
        let (_dir, state) = test_state();

        state
            .register_app("myapp", "myapp.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        state.set_secret("myapp", "API_KEY", "sk-123").unwrap();
        state
            .set_secret("myapp", "DB_URL", "sqlite:data.db")
            .unwrap();

        let secrets = state.get_secrets("myapp").unwrap();
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets[0], ("API_KEY".into(), "sk-123".into()));
        assert_eq!(secrets[1], ("DB_URL".into(), "sqlite:data.db".into()));

        // Update
        state.set_secret("myapp", "API_KEY", "sk-456").unwrap();
        let secrets = state.get_secrets("myapp").unwrap();
        assert_eq!(secrets[0].1, "sk-456");

        // Remove
        assert!(state.remove_secret("myapp", "API_KEY").unwrap());
        assert!(!state.remove_secret("myapp", "NONEXISTENT").unwrap());

        let secrets = state.get_secrets("myapp").unwrap();
        assert_eq!(secrets.len(), 1);
    }

    #[test]
    fn secrets_file_permissions() {
        let (_dir, state) = test_state();

        state
            .register_app("myapp", "myapp.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        state.set_secret("myapp", "KEY", "value").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(state.secrets_path("myapp"))
                .unwrap()
                .permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn list_active_apps_only_returns_deployed() {
        let (_dir, state) = test_state();

        // App with no deploy
        state
            .register_app("app1", "app1.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        // App with a deploy (current symlink)
        state
            .register_app("app2", "app2.com", "binary", None, None, "blue-green", 5)
            .unwrap();
        let release = state.app_dir("app2").join("releases").join("20260305-001");
        std::fs::create_dir_all(&release).unwrap();
        std::os::unix::fs::symlink(&release, state.app_dir("app2").join("current")).unwrap();

        let active = state.list_active_apps().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "app2");
    }
}
