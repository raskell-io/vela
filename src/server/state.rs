use anyhow::Result;
use rusqlite::Connection;

use crate::config::ServerConfig;

pub struct ServerState {
    db: Connection,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub domain: String,
    pub current_release: String,
    pub status: String,
}

impl ServerState {
    pub fn open(config: &ServerConfig) -> Result<Self> {
        let db_path = config.db_path();

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db = Connection::open(&db_path)?;
        let state = Self { db };
        state.migrate()?;
        Ok(state)
    }

    fn migrate(&self) -> Result<()> {
        self.db.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS apps (
                name            TEXT PRIMARY KEY,
                domain          TEXT NOT NULL UNIQUE,
                app_type        TEXT NOT NULL DEFAULT 'binary',
                binary_name     TEXT,
                health_path     TEXT,
                deploy_strategy TEXT NOT NULL DEFAULT 'blue-green',
                drain_seconds   INTEGER NOT NULL DEFAULT 5,
                port            INTEGER,
                created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS releases (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                app_name        TEXT NOT NULL REFERENCES apps(name),
                release_id      TEXT NOT NULL,
                status          TEXT NOT NULL DEFAULT 'pending',
                created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                activated_at    TEXT,
                UNIQUE(app_name, release_id)
            );

            CREATE TABLE IF NOT EXISTS secrets (
                app_name        TEXT NOT NULL REFERENCES apps(name),
                key             TEXT NOT NULL,
                value           TEXT NOT NULL,
                updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (app_name, key)
            );
            ",
        )?;
        Ok(())
    }

    pub fn list_apps(&self) -> Result<Vec<AppInfo>> {
        let mut stmt = self.db.prepare(
            "
            SELECT a.name, a.domain,
                   COALESCE(r.release_id, 'none') as current_release,
                   COALESCE(r.status, 'no-release') as status
            FROM apps a
            LEFT JOIN releases r ON r.app_name = a.name
                AND r.status = 'active'
            ORDER BY a.name
            ",
        )?;

        let apps = stmt
            .query_map([], |row| {
                Ok(AppInfo {
                    name: row.get(0)?,
                    domain: row.get(1)?,
                    current_release: row.get(2)?,
                    status: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

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
        self.db.execute(
            "INSERT INTO apps (name, domain, app_type, binary_name, health_path, deploy_strategy, drain_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
                domain = excluded.domain,
                app_type = excluded.app_type,
                binary_name = excluded.binary_name,
                health_path = excluded.health_path,
                deploy_strategy = excluded.deploy_strategy,
                drain_seconds = excluded.drain_seconds,
                updated_at = datetime('now')",
            rusqlite::params![name, domain, app_type, binary_name, health_path, deploy_strategy, drain_seconds],
        )?;
        Ok(())
    }

    pub fn create_release(&self, app_name: &str, release_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO releases (app_name, release_id, status) VALUES (?1, ?2, 'pending')",
            rusqlite::params![app_name, release_id],
        )?;
        Ok(())
    }

    pub fn activate_release(&self, app_name: &str, release_id: &str) -> Result<()> {
        // Deactivate current active release
        self.db.execute(
            "UPDATE releases SET status = 'inactive' WHERE app_name = ?1 AND status = 'active'",
            rusqlite::params![app_name],
        )?;
        // Activate new release
        self.db.execute(
            "UPDATE releases SET status = 'active', activated_at = datetime('now')
             WHERE app_name = ?1 AND release_id = ?2",
            rusqlite::params![app_name, release_id],
        )?;
        Ok(())
    }

    pub fn fail_release(&self, app_name: &str, release_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE releases SET status = 'failed' WHERE app_name = ?1 AND release_id = ?2",
            rusqlite::params![app_name, release_id],
        )?;
        Ok(())
    }

    pub fn get_active_release(&self, app_name: &str) -> Result<Option<String>> {
        let result = self.db.query_row(
            "SELECT release_id FROM releases WHERE app_name = ?1 AND status = 'active'",
            rusqlite::params![app_name],
            |row| row.get(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_previous_release(&self, app_name: &str) -> Result<Option<String>> {
        let result = self.db.query_row(
            "SELECT release_id FROM releases
             WHERE app_name = ?1 AND status = 'inactive'
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![app_name],
            |row| row.get(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_secret(&self, app_name: &str, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO secrets (app_name, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(app_name, key) DO UPDATE SET
                value = excluded.value,
                updated_at = datetime('now')",
            rusqlite::params![app_name, key, value],
        )?;
        Ok(())
    }

    pub fn get_secrets(&self, app_name: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .db
            .prepare("SELECT key, value FROM secrets WHERE app_name = ?1 ORDER BY key")?;
        let secrets = stmt
            .query_map(rusqlite::params![app_name], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(secrets)
    }

    pub fn remove_secret(&self, app_name: &str, key: &str) -> Result<bool> {
        let changed = self.db.execute(
            "DELETE FROM secrets WHERE app_name = ?1 AND key = ?2",
            rusqlite::params![app_name, key],
        )?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ServerConfig {
        let dir = tempfile::tempdir().unwrap();
        ServerConfig {
            data_dir: dir.into_path(),
            ..Default::default()
        }
    }

    #[test]
    fn open_and_migrate() {
        let config = test_config();
        let state = ServerState::open(&config).unwrap();
        let apps = state.list_apps().unwrap();
        assert!(apps.is_empty());
    }

    #[test]
    fn register_and_list_app() {
        let config = test_config();
        let state = ServerState::open(&config).unwrap();

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
    }

    #[test]
    fn release_lifecycle() {
        let config = test_config();
        let state = ServerState::open(&config).unwrap();

        state
            .register_app("myapp", "myapp.com", "binary", None, None, "blue-green", 5)
            .unwrap();

        state.create_release("myapp", "20260305-001").unwrap();
        state.activate_release("myapp", "20260305-001").unwrap();

        assert_eq!(
            state.get_active_release("myapp").unwrap(),
            Some("20260305-001".into())
        );

        // Deploy a new release
        state.create_release("myapp", "20260305-002").unwrap();
        state.activate_release("myapp", "20260305-002").unwrap();

        assert_eq!(
            state.get_active_release("myapp").unwrap(),
            Some("20260305-002".into())
        );
        assert_eq!(
            state.get_previous_release("myapp").unwrap(),
            Some("20260305-001".into())
        );
    }

    #[test]
    fn secrets_crud() {
        let config = test_config();
        let state = ServerState::open(&config).unwrap();

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
}
