use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::process::ProcessManager;
use super::proxy::RouteTable;
use super::service::ServiceManager;
use super::state::ServerState;
use crate::config::{AppType, DeployStrategy, ServerConfig};
use crate::health::HealthCheck;

/// Shared parameters for deploy operations.
struct DeployParams<'a> {
    process_manager: &'a Arc<Mutex<ProcessManager>>,
    route_table: &'a RouteTable,
    app: &'a str,
    release_dir: &'a Path,
    binary_name: &'a str,
    app_type: AppType,
    data_dir: &'a Path,
    env_vars: &'a [(String, String)],
    health_path: Option<&'a str>,
    drain_seconds: u32,
    domain: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command")]
pub enum DaemonRequest {
    #[serde(rename = "deploy")]
    Deploy {
        app: String,
        release_dir: PathBuf,
        binary_name: String,
        app_type: String,
        strategy: String,
        data_dir: PathBuf,
        env_vars: Vec<(String, String)>,
        health_path: Option<String>,
        drain_seconds: u32,
        domain: String,
        #[serde(default)]
        services: HashMap<String, toml::Value>,
    },
    #[serde(rename = "stop")]
    Stop { app: String, domain: String },
    #[serde(rename = "status")]
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub success: bool,
    pub message: String,
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<AppStatusEntry>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppStatusEntry {
    pub name: String,
    pub domain: String,
    pub release: String,
    pub strategy: String,
    pub pid: Option<u32>,
    pub port: u16,
    pub uptime_seconds: u64,
    pub health: String,
}

/// Start the IPC server on a Unix domain socket.
pub async fn start_ipc_server(
    sock_path: &Path,
    process_manager: Arc<Mutex<ProcessManager>>,
    route_table: RouteTable,
    service_manager: Arc<Mutex<ServiceManager>>,
    data_dir: PathBuf,
) -> Result<()> {
    // Remove stale socket from a previous run
    let _ = std::fs::remove_file(sock_path);

    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("failed to bind IPC socket at {}", sock_path.display()))?;

    // Restrict socket permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::info!(path = %sock_path.display(), "IPC server listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let pm = process_manager.clone();
        let rt = route_table.clone();
        let sm = service_manager.clone();
        let dd = data_dir.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, pm, rt, sm, dd).await {
                tracing::error!(err = %e, "IPC connection error");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    process_manager: Arc<Mutex<ProcessManager>>,
    route_table: RouteTable,
    service_manager: Arc<Mutex<ServiceManager>>,
    data_dir: PathBuf,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    let request: DaemonRequest =
        serde_json::from_str(line.trim()).context("failed to parse IPC request")?;

    let response = match request {
        DaemonRequest::Deploy {
            app,
            release_dir,
            binary_name,
            app_type,
            strategy,
            data_dir,
            mut env_vars,
            health_path,
            drain_seconds,
            domain,
            services,
        } => {
            // Provision services and inject their env vars
            if !services.is_empty() {
                let mut sm = service_manager.lock().await;
                let mut svc_failed = None;
                for (svc_type, svc_config) in &services {
                    match sm.ensure_service(svc_type, svc_config).await {
                        Ok(svc_vars) => {
                            for (k, v) in svc_vars {
                                if !env_vars.iter().any(|(ek, _)| ek == &k) {
                                    env_vars.push((k, v));
                                }
                            }
                        }
                        Err(e) => {
                            svc_failed = Some(DaemonResponse {
                                success: false,
                                message: format!("failed to provision service {svc_type}: {e}"),
                                port: None,
                                apps: None,
                            });
                            break;
                        }
                    }
                }
                if let Some(resp) = svc_failed {
                    resp
                } else {
                    handle_deploy(
                        &process_manager,
                        &route_table,
                        &app,
                        &release_dir,
                        &binary_name,
                        &app_type,
                        &strategy,
                        &data_dir,
                        &env_vars,
                        health_path.as_deref(),
                        drain_seconds,
                        &domain,
                    )
                    .await
                }
            } else {
                handle_deploy(
                    &process_manager,
                    &route_table,
                    &app,
                    &release_dir,
                    &binary_name,
                    &app_type,
                    &strategy,
                    &data_dir,
                    &env_vars,
                    health_path.as_deref(),
                    drain_seconds,
                    &domain,
                )
                .await
            }
        }
        DaemonRequest::Stop { app, domain } => {
            handle_stop(&process_manager, &route_table, &app, &domain).await
        }
        DaemonRequest::Status => {
            handle_status(&process_manager, &data_dir).await
        }
    };

    let resp_json = serde_json::to_string(&response)?;
    writer.write_all(resp_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_deploy(
    process_manager: &Arc<Mutex<ProcessManager>>,
    route_table: &RouteTable,
    app: &str,
    release_dir: &Path,
    binary_name: &str,
    app_type: &str,
    strategy: &str,
    data_dir: &Path,
    env_vars: &[(String, String)],
    health_path: Option<&str>,
    drain_seconds: u32,
    domain: &str,
) -> DaemonResponse {
    let params = DeployParams {
        process_manager,
        route_table,
        app,
        release_dir,
        binary_name,
        app_type: AppType::from_str_loose(app_type),
        data_dir,
        env_vars,
        health_path,
        drain_seconds,
        domain,
    };

    match DeployStrategy::from_str_loose(strategy) {
        DeployStrategy::BlueGreen => deploy_blue_green(&params).await,
        DeployStrategy::Sequential => deploy_sequential(&params).await,
    }
}

/// Blue-green: start new → health check → swap traffic → drain old.
/// Zero downtime. Two instances run briefly during the swap.
async fn deploy_blue_green(params: &DeployParams<'_>) -> DaemonResponse {
    let app = params.app;

    // Start new instance alongside old
    let port = {
        let mut pm = params.process_manager.lock().await;
        match pm
            .start(
                app,
                params.release_dir,
                params.binary_name,
                params.app_type,
                params.data_dir,
                params.env_vars,
            )
            .await
        {
            Ok(port) => port,
            Err(e) => {
                return DaemonResponse {
                    success: false,
                    message: format!("failed to start app: {e}"),
                    port: None,
                    apps: None,
                };
            }
        }
    };

    tracing::info!(
        app,
        port,
        strategy = "blue-green",
        "new instance started, running health check"
    );

    // Health check
    if let Err(resp) = run_health_check(params.process_manager, app, port, params.health_path).await
    {
        return resp;
    }

    // Swap: drain old instance, promote new
    {
        let mut pm = params.process_manager.lock().await;
        if let Err(e) = pm.swap(app, params.drain_seconds).await {
            tracing::warn!(app, err = %e, "swap warning (may be first deploy)");
        }
    }

    params.route_table.set(params.domain, port);
    tracing::info!(
        app,
        port,
        domain = params.domain,
        strategy = "blue-green",
        "deploy activated"
    );

    DaemonResponse {
        success: true,
        message: format!("deployed {app} on port {port}"),
        port: Some(port),
        apps: None,
    }
}

/// Sequential: stop old → start new → health check → activate.
/// Sub-second blip. Use for SQLite apps to avoid write contention.
async fn deploy_sequential(params: &DeployParams<'_>) -> DaemonResponse {
    let app = params.app;

    // Stop old instance first (removes route so no traffic during gap)
    {
        let mut pm = params.process_manager.lock().await;
        if pm.active_port(app).is_some() {
            params.route_table.remove(params.domain);
            tracing::info!(
                app,
                strategy = "sequential",
                "stopping old instance before starting new"
            );
            if let Err(e) = pm.stop(app).await {
                tracing::warn!(app, err = %e, "stop warning (may be first deploy)");
            }
        }
    }

    // Start new instance
    let port = {
        let mut pm = params.process_manager.lock().await;
        match pm
            .start(
                app,
                params.release_dir,
                params.binary_name,
                params.app_type,
                params.data_dir,
                params.env_vars,
            )
            .await
        {
            Ok(port) => port,
            Err(e) => {
                return DaemonResponse {
                    success: false,
                    message: format!("failed to start app: {e}"),
                    port: None,
                    apps: None,
                };
            }
        }
    };

    tracing::info!(
        app,
        port,
        strategy = "sequential",
        "new instance started, running health check"
    );

    // Health check
    if let Err(resp) = run_health_check(params.process_manager, app, port, params.health_path).await
    {
        return resp;
    }

    // Promote pending to active (no swap needed — old was already stopped)
    {
        let mut pm = params.process_manager.lock().await;
        pm.promote_pending_to_active(app);
    }

    params.route_table.set(params.domain, port);
    tracing::info!(
        app,
        port,
        domain = params.domain,
        strategy = "sequential",
        "deploy activated"
    );

    DaemonResponse {
        success: true,
        message: format!("deployed {app} on port {port}"),
        port: Some(port),
        apps: None,
    }
}

/// Run health check on a pending instance. Returns Err(DaemonResponse) on failure.
async fn run_health_check(
    process_manager: &Arc<Mutex<ProcessManager>>,
    app: &str,
    port: u16,
    health_path: Option<&str>,
) -> Result<(), DaemonResponse> {
    if let Some(health) = health_path {
        let url = format!("http://127.0.0.1:{port}{health}");
        let hc = HealthCheck::new(url);
        match hc.wait_until_healthy().await {
            Ok(()) => {
                tracing::info!(app, port, "health check passed");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(app, port, err = %e, "health check failed");
                let mut pm = process_manager.lock().await;
                let _ = pm.abort_pending(app).await;
                Err(DaemonResponse {
                    success: false,
                    message: format!("health check failed: {e}"),
                    port: None,
                    apps: None,
                })
            }
        }
    } else {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        Ok(())
    }
}

async fn handle_stop(
    process_manager: &Arc<Mutex<ProcessManager>>,
    route_table: &RouteTable,
    app: &str,
    domain: &str,
) -> DaemonResponse {
    route_table.remove(domain);

    let mut pm = process_manager.lock().await;
    match pm.stop(app).await {
        Ok(()) => DaemonResponse {
            success: true,
            message: format!("stopped {app}"),
            port: None,
            apps: None,
        },
        Err(e) => DaemonResponse {
            success: false,
            message: format!("failed to stop {app}: {e}"),
            port: None,
            apps: None,
        },
    }
}

async fn handle_status(
    process_manager: &Arc<Mutex<ProcessManager>>,
    data_dir: &Path,
) -> DaemonResponse {
    let pm = process_manager.lock().await;
    let active = pm.list_active_details();
    drop(pm);

    let config = ServerConfig {
        data_dir: data_dir.to_path_buf(),
        ..Default::default()
    };
    let state = match ServerState::open(&config) {
        Ok(s) => s,
        Err(e) => {
            return DaemonResponse {
                success: false,
                message: format!("failed to read server state: {e}"),
                port: None,
                apps: None,
            };
        }
    };

    let mut entries = Vec::new();
    for proc_info in &active {
        let (domain, strategy, health_path) =
            match state.get_app(&proc_info.app_name) {
                Ok(Some(app)) => (
                    app.domain,
                    app.deploy_strategy,
                    app.health_path,
                ),
                _ => (String::new(), "blue-green".into(), None),
            };

        let health = probe_health(proc_info.port, health_path.as_deref()).await;
        let uptime = std::time::SystemTime::now()
            .duration_since(proc_info.started_at)
            .unwrap_or_default()
            .as_secs();

        entries.push(AppStatusEntry {
            name: proc_info.app_name.clone(),
            domain,
            release: proc_info.release_id.clone(),
            strategy,
            pid: proc_info.pid,
            port: proc_info.port,
            uptime_seconds: uptime,
            health,
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));

    DaemonResponse {
        success: true,
        message: format!("{} app(s) running", entries.len()),
        port: None,
        apps: Some(entries),
    }
}

/// Quick single-probe health check for status queries.
async fn probe_health(port: u16, health_path: Option<&str>) -> String {
    let Some(path) = health_path else {
        return "unknown".to_string();
    };
    let url = format!("http://127.0.0.1:{port}{path}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build();
    match client {
        Ok(c) => match c.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => "healthy".to_string(),
            _ => "unhealthy".to_string(),
        },
        Err(_) => "unhealthy".to_string(),
    }
}

/// Send a command to the running daemon via Unix socket.
/// Used by `_deploy` and `_rollback` commands.
pub async fn send_command(sock_path: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let stream = UnixStream::connect(sock_path).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to connect to vela daemon at {}: {e}\nis `vela serve` running?",
            sock_path.display()
        )
    })?;

    let (reader, mut writer) = stream.into_split();

    let req_json = serde_json::to_string(request)?;
    writer.write_all(req_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    // Signal we're done writing so the server doesn't wait for more input
    drop(writer);

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: DaemonResponse =
        serde_json::from_str(line.trim()).context("failed to parse daemon response")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_deploy_request() {
        let req = DaemonRequest::Deploy {
            app: "myapp".into(),
            release_dir: PathBuf::from("/var/vela/apps/myapp/releases/001"),
            binary_name: "myapp".into(),
            app_type: "binary".into(),
            strategy: "blue-green".into(),
            data_dir: PathBuf::from("/var/vela/apps/myapp/data"),
            env_vars: vec![("PORT".into(), "3000".into())],
            health_path: Some("/health".into()),
            drain_seconds: 5,
            domain: "myapp.com".into(),
            services: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::Deploy { app, domain, .. } => {
                assert_eq!(app, "myapp");
                assert_eq!(domain, "myapp.com");
            }
            _ => panic!("expected Deploy"),
        }
    }

    #[test]
    fn serialize_deploy_with_services() {
        let mut services = HashMap::new();
        services.insert(
            "postgres".into(),
            toml::Value::try_from(toml::toml! {
                version = "17"
                databases = ["mydb"]
            })
            .unwrap(),
        );

        let req = DaemonRequest::Deploy {
            app: "myapp".into(),
            release_dir: PathBuf::from("/var/vela/apps/myapp/releases/001"),
            binary_name: "myapp".into(),
            app_type: "binary".into(),
            strategy: "blue-green".into(),
            data_dir: PathBuf::from("/var/vela/apps/myapp/data"),
            env_vars: vec![],
            health_path: None,
            drain_seconds: 5,
            domain: "myapp.com".into(),
            services,
        };

        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::Deploy { services, .. } => {
                assert!(services.contains_key("postgres"));
            }
            _ => panic!("expected Deploy"),
        }
    }

    #[test]
    fn serialize_response() {
        let resp = DaemonResponse {
            success: true,
            message: "deployed myapp on port 10001".into(),
            port: Some(10001),
            apps: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.port, Some(10001));
    }

    #[test]
    fn serialize_status_request() {
        let req = DaemonRequest::Status;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("status"));
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonRequest::Status));
    }

    #[test]
    fn serialize_status_response() {
        let resp = DaemonResponse {
            success: true,
            message: "2 app(s) running".into(),
            port: None,
            apps: Some(vec![AppStatusEntry {
                name: "cyanea".into(),
                domain: "cyanea.bio".into(),
                release: "20260305-001".into(),
                strategy: "sequential".into(),
                pid: Some(12345),
                port: 10001,
                uptime_seconds: 3600,
                health: "healthy".into(),
            }]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("cyanea"));
        assert!(json.contains("12345"));
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.apps.unwrap().len(), 1);
    }

    #[test]
    fn response_without_apps_omits_field() {
        let resp = DaemonResponse {
            success: true,
            message: "deployed".into(),
            port: Some(10001),
            apps: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("apps"));
    }
}
