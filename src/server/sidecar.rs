//! Generic sidecar services: any command declared in `Vela.toml` that Vela
//! starts before the app, health-checks, supervises and restarts.
//!
//! ```toml
//! [services.meilisearch]
//! command = "/usr/local/bin/meilisearch"
//! args = ["--db-path", "${data_dir}", "--http-addr", "127.0.0.1:7700"]
//! health = "http://127.0.0.1:7700/health"
//! restart = "always"            # or "on-failure" / "never"
//!
//! [services.meilisearch.env]
//! MEILI_MASTER_KEY = "${secret:MEILI_MASTER_KEY}"
//!
//! [services.meilisearch.export]  # env vars injected into the app
//! MEILI_URL = "http://127.0.0.1:7700"
//! ```
//!
//! State on disk, per service, under `/var/vela/services/<name>/`:
//! `service.toml` (the spec, mode 0600), `data/` and `service.log`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use crate::health::HealthCheck;

/// Names taken by the curated service providers.
const RESERVED_NAMES: [&str; 2] = ["postgres", "nats"];

/// How long a stopped service gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Declared in `[services.<name>]` with a `command` key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// HTTP URL that must answer 2xx before the service counts as up.
    pub health: Option<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Environment variables injected into apps that declare this service.
    #[serde(default)]
    pub export: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl RestartPolicy {
    fn should_restart(self, success: bool) -> bool {
        match self {
            Self::Always => true,
            Self::OnFailure => !success,
            Self::Never => false,
        }
    }
}

/// Whether a `[services.<name>]` table describes a generic sidecar.
pub fn is_sidecar(config: &toml::Value) -> bool {
    config.get("command").is_some()
}

/// Sidecar names become directory names: lowercase letters, digits, '-', '_'.
pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        anyhow::bail!("invalid service name '{name}': use lowercase letters, digits, '-' and '_'");
    }
    if RESERVED_NAMES.contains(&name) {
        anyhow::bail!(
            "service name '{name}' is reserved for the built-in provider; \
             drop `command` to use it, or pick another name"
        );
    }
    Ok(())
}

/// Replace `${secret:KEY}` references in every string the service passes on
/// (env, args, exports). Unknown keys are left as written.
pub fn resolve_secrets(spec: &mut SidecarSpec, secrets: &[(String, String)]) {
    let resolve = |s: &mut String| {
        for (key, value) in secrets {
            let placeholder = format!("${{secret:{key}}}");
            if s.contains(&placeholder) {
                *s = s.replace(&placeholder, value);
            }
        }
    };
    spec.env.values_mut().for_each(resolve);
    spec.args.iter_mut().for_each(resolve);
    spec.export.values_mut().for_each(resolve);
}

/// Status of one sidecar for `vela status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarStatus {
    pub name: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime_seconds: u64,
    pub restarts: u32,
}

struct Sidecar {
    spec: SidecarSpec,
    child: Option<Child>,
    started_at: SystemTime,
    restarts: u32,
    /// False once the restart policy or a shutdown says it should stay down.
    wants_running: bool,
}

/// Owns every running sidecar. Lives inside the daemon's `ServiceManager`.
pub struct SidecarManager {
    services_dir: PathBuf,
    sidecars: HashMap<String, Sidecar>,
}

impl SidecarManager {
    pub fn new(services_dir: &Path) -> Self {
        Self {
            services_dir: services_dir.to_path_buf(),
            sidecars: HashMap::new(),
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.services_dir.join(name)
    }

    /// Make sure the sidecar runs with this spec. Restarts it if the spec
    /// changed. Returns the env vars to inject into the app.
    pub async fn ensure(&mut self, name: &str, spec: SidecarSpec) -> Result<Vec<(String, String)>> {
        validate_name(name)?;

        let running_same = self
            .sidecars
            .get_mut(name)
            .is_some_and(|s| s.spec == spec && is_running(&mut s.child));

        if !running_same {
            if let Some(mut old) = self.sidecars.remove(name) {
                tracing::info!(service = name, "service spec changed, restarting");
                stop_child(name, &mut old.child).await;
            }

            let dir = self.dir(name);
            std::fs::create_dir_all(dir.join("data"))?;
            save_spec(&dir.join("service.toml"), &spec)?;

            let child = self.start(name, &spec).await?;
            self.sidecars.insert(
                name.to_string(),
                Sidecar {
                    spec: spec.clone(),
                    child: Some(child),
                    started_at: SystemTime::now(),
                    restarts: 0,
                    wants_running: true,
                },
            );
        }

        Ok(self.exports(name, &spec))
    }

    /// Start every sidecar that has a saved spec (daemon start-up).
    pub async fn restore(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.services_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let spec_path = entry.path().join("service.toml");
            if !spec_path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            match load_spec(&spec_path) {
                Ok(spec) => {
                    tracing::info!(service = %name, "restoring service");
                    if let Err(e) = self.ensure(&name, spec).await {
                        tracing::error!(service = %name, err = %e, "failed to restore service");
                    }
                }
                Err(e) => {
                    tracing::error!(service = %name, err = %e, "invalid saved service spec");
                }
            }
        }
    }

    /// Restart sidecars that exited, according to their restart policy.
    /// A restart that fails is retried on the next supervision tick.
    pub async fn check_and_restart(&mut self) {
        let names: Vec<String> = self.sidecars.keys().cloned().collect();
        for name in names {
            let Some(sidecar) = self.sidecars.get_mut(&name) else {
                continue;
            };
            match sidecar.child.as_mut().map(Child::try_wait) {
                Some(Ok(None)) => continue,
                Some(Err(e)) => {
                    tracing::warn!(service = %name, err = %e, "failed to check service status");
                    continue;
                }
                Some(Ok(Some(status))) => {
                    sidecar.child = None;
                    if !sidecar.spec.restart.should_restart(status.success()) {
                        sidecar.wants_running = false;
                        tracing::warn!(service = %name, ?status, "service exited, not restarting (policy)");
                        continue;
                    }
                    tracing::warn!(service = %name, ?status, "service exited, restarting");
                }
                None if !sidecar.wants_running => continue,
                None => tracing::info!(service = %name, "retrying service start"),
            }

            let spec = sidecar.spec.clone();
            match self.start(&name, &spec).await {
                Ok(child) => {
                    if let Some(s) = self.sidecars.get_mut(&name) {
                        s.child = Some(child);
                        s.started_at = SystemTime::now();
                        s.restarts += 1;
                    }
                }
                Err(e) => tracing::error!(service = %name, err = %e, "failed to restart service"),
            }
        }
    }

    /// Stop every sidecar (daemon shutdown): SIGTERM, then SIGKILL after a grace period.
    pub async fn stop_all(&mut self) {
        for (name, sidecar) in &mut self.sidecars {
            sidecar.wants_running = false;
            stop_child(name, &mut sidecar.child).await;
        }
    }

    pub fn status(&mut self) -> Vec<SidecarStatus> {
        let mut out: Vec<SidecarStatus> = self
            .sidecars
            .iter_mut()
            .map(|(name, s)| {
                let running = is_running(&mut s.child);
                SidecarStatus {
                    name: name.clone(),
                    running,
                    pid: if running {
                        s.child.as_ref().and_then(Child::id)
                    } else {
                        None
                    },
                    uptime_seconds: if running {
                        SystemTime::now()
                            .duration_since(s.started_at)
                            .unwrap_or_default()
                            .as_secs()
                    } else {
                        0
                    },
                    restarts: s.restarts,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn exports(&self, name: &str, spec: &SidecarSpec) -> Vec<(String, String)> {
        let data_dir = self.dir(name).join("data");
        spec.export
            .iter()
            .map(|(k, v)| (k.clone(), expand_data_dir(v, &data_dir)))
            .collect()
    }

    async fn start(&self, name: &str, spec: &SidecarSpec) -> Result<Child> {
        let dir = self.dir(name);
        let data_dir = dir.join("data");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("service.log"))
            .with_context(|| format!("failed to open log for service {name}"))?;

        let mut child = Command::new(expand_data_dir(&spec.command, &data_dir))
            .args(spec.args.iter().map(|a| expand_data_dir(a, &data_dir)))
            .envs(
                spec.env
                    .iter()
                    .map(|(k, v)| (k.clone(), expand_data_dir(v, &data_dir))),
            )
            .env("VELA_DATA_DIR", &data_dir)
            .env("VELA_SERVICE_NAME", name)
            .current_dir(&data_dir)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("failed to start service {name} ({})", spec.command))?;

        tracing::info!(service = name, pid = ?child.id(), "service started");

        let ready = match &spec.health {
            Some(url) => HealthCheck::new(url.clone())
                .wait_until_healthy()
                .await
                .map_err(|e| anyhow::anyhow!("service {name} failed its health check: {e}")),
            None => {
                // No health check: it must at least survive its first second.
                tokio::time::sleep(Duration::from_secs(1)).await;
                match child.try_wait() {
                    Ok(None) => Ok(()),
                    Ok(Some(status)) => Err(anyhow::anyhow!(
                        "service {name} exited right after start ({status}); see {}",
                        dir.join("service.log").display()
                    )),
                    Err(e) => Err(e.into()),
                }
            }
        };

        if let Err(e) = ready {
            let mut child = Some(child);
            stop_child(name, &mut child).await;
            return Err(e);
        }
        Ok(child)
    }
}

fn expand_data_dir(value: &str, data_dir: &Path) -> String {
    value.replace("${data_dir}", &data_dir.to_string_lossy())
}

fn is_running(child: &mut Option<Child>) -> bool {
    child
        .as_mut()
        .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
}

async fn stop_child(name: &str, child: &mut Option<Child>) {
    let Some(mut c) = child.take() else {
        return;
    };
    if let Some(pid) = c.id() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        if tokio::time::timeout(STOP_GRACE, c.wait()).await.is_ok() {
            tracing::info!(service = name, "service stopped");
            return;
        }
        tracing::warn!(service = name, "service ignored SIGTERM, killing");
    }
    let _ = c.kill().await;
}

fn save_spec(path: &Path, spec: &SidecarSpec) -> Result<()> {
    let content = toml::to_string_pretty(spec).context("failed to serialize service spec")?;
    std::fs::write(path, content)?;
    // The spec can hold resolved secrets.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_spec(path: &Path) -> Result<SidecarSpec> {
    let content = std::fs::read_to_string(path)?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str) -> SidecarSpec {
        SidecarSpec {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            health: None,
            restart: RestartPolicy::Always,
            export: BTreeMap::new(),
        }
    }

    #[test]
    fn parse_spec_from_manifest_table() {
        let value: toml::Value = toml::from_str(
            r#"
command = "/usr/local/bin/meilisearch"
args = ["--db-path", "${data_dir}"]
health = "http://127.0.0.1:7700/health"
restart = "on-failure"
env = { MEILI_MASTER_KEY = "${secret:MEILI_KEY}" }
export = { MEILI_URL = "http://127.0.0.1:7700" }
"#,
        )
        .unwrap();
        assert!(is_sidecar(&value));
        let spec: SidecarSpec = value.try_into().unwrap();
        assert_eq!(spec.restart, RestartPolicy::OnFailure);
        assert_eq!(spec.export["MEILI_URL"], "http://127.0.0.1:7700");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let value: toml::Value = toml::from_str("command = \"x\"\nrestrat = \"never\"").unwrap();
        assert!(SidecarSpec::deserialize(value).is_err());
    }

    #[test]
    fn curated_tables_are_not_sidecars() {
        let value: toml::Value = toml::from_str("version = \"17\"").unwrap();
        assert!(!is_sidecar(&value));
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("meilisearch").is_ok());
        assert!(validate_name("redis-cache_1").is_ok());
        assert!(validate_name("Redis").is_err());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("postgres").is_err());
    }

    #[test]
    fn restart_policy() {
        assert!(RestartPolicy::Always.should_restart(true));
        assert!(RestartPolicy::OnFailure.should_restart(false));
        assert!(!RestartPolicy::OnFailure.should_restart(true));
        assert!(!RestartPolicy::Never.should_restart(false));
    }

    #[test]
    fn secrets_resolve_in_env_args_and_exports() {
        let mut s = spec("x");
        s.env.insert("KEY".into(), "${secret:K}".into());
        s.args.push("--key=${secret:K}".into());
        s.export
            .insert("URL".into(), "redis://:${secret:K}@localhost".into());
        s.env.insert("OTHER".into(), "${secret:MISSING}".into());
        resolve_secrets(&mut s, &[("K".into(), "v".into())]);
        assert_eq!(s.env["KEY"], "v");
        assert_eq!(s.args[0], "--key=v");
        assert_eq!(s.export["URL"], "redis://:v@localhost");
        assert_eq!(s.env["OTHER"], "${secret:MISSING}");
    }

    #[tokio::test]
    async fn ensure_starts_supervises_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SidecarManager::new(dir.path());

        let mut s = spec("/bin/sh");
        s.args = vec!["-c".into(), "exec sleep 30".into()];
        s.export.insert("DIR".into(), "${data_dir}".into());

        let exports = mgr.ensure("sleeper", s.clone()).await.unwrap();
        let data = dir.path().join("sleeper").join("data");
        assert_eq!(
            exports,
            vec![("DIR".to_string(), data.to_string_lossy().to_string())]
        );
        assert!(dir.path().join("sleeper/service.toml").is_file());

        let status = mgr.status();
        assert!(status[0].running);
        let pid = status[0].pid;

        // Same spec again: no restart
        mgr.ensure("sleeper", s.clone()).await.unwrap();
        assert_eq!(mgr.status()[0].pid, pid);

        // Crash: supervision restarts it
        let pid = pid.unwrap() as i32;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        // Poll until supervision has seen the exit (no fixed sleep in tests)
        for _ in 0..100 {
            mgr.check_and_restart().await;
            if mgr.status()[0].restarts == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let status = mgr.status();
        assert!(status[0].running);
        assert_eq!(status[0].restarts, 1);

        mgr.stop_all().await;
        assert!(!mgr.status()[0].running);
    }

    #[tokio::test]
    async fn a_service_that_exits_at_once_fails_the_deploy() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SidecarManager::new(dir.path());
        let err = mgr.ensure("broken", spec("/bin/false")).await.unwrap_err();
        assert!(err.to_string().contains("exited right after start"));
    }

    #[tokio::test]
    async fn restore_starts_saved_services() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec("/bin/sh");
        s.args = vec!["-c".into(), "exec sleep 30".into()];
        {
            let mut mgr = SidecarManager::new(dir.path());
            mgr.ensure("sleeper", s).await.unwrap();
            mgr.stop_all().await;
        }
        let mut mgr = SidecarManager::new(dir.path());
        mgr.restore().await;
        assert!(mgr.status()[0].running);
        mgr.stop_all().await;
    }
}
