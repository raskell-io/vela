use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::process::ProcessManager;
use super::proxy::RouteTable;
use crate::config::{AppType, DeployStrategy};
use crate::health::HealthCheck;

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
    },
    #[serde(rename = "stop")]
    Stop { app: String, domain: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub success: bool,
    pub message: String,
    pub port: Option<u16>,
}

/// Start the IPC server on a Unix domain socket.
pub async fn start_ipc_server(
    sock_path: &Path,
    process_manager: Arc<Mutex<ProcessManager>>,
    route_table: RouteTable,
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

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, pm, rt).await {
                tracing::error!(err = %e, "IPC connection error");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    process_manager: Arc<Mutex<ProcessManager>>,
    route_table: RouteTable,
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
            env_vars,
            health_path,
            drain_seconds,
            domain,
        } => {
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
        DaemonRequest::Stop { app, domain } => {
            handle_stop(&process_manager, &route_table, &app, &domain).await
        }
    };

    let resp_json = serde_json::to_string(&response)?;
    writer.write_all(resp_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}

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
    let app_type_enum = AppType::from_str_loose(app_type);
    let strategy_enum = DeployStrategy::from_str_loose(strategy);

    match strategy_enum {
        DeployStrategy::BlueGreen => {
            deploy_blue_green(
                process_manager,
                route_table,
                app,
                release_dir,
                binary_name,
                app_type_enum,
                data_dir,
                env_vars,
                health_path,
                drain_seconds,
                domain,
            )
            .await
        }
        DeployStrategy::Sequential => {
            deploy_sequential(
                process_manager,
                route_table,
                app,
                release_dir,
                binary_name,
                app_type_enum,
                data_dir,
                env_vars,
                health_path,
                drain_seconds,
                domain,
            )
            .await
        }
    }
}

/// Blue-green: start new → health check → swap traffic → drain old.
/// Zero downtime. Two instances run briefly during the swap.
async fn deploy_blue_green(
    process_manager: &Arc<Mutex<ProcessManager>>,
    route_table: &RouteTable,
    app: &str,
    release_dir: &Path,
    binary_name: &str,
    app_type: AppType,
    data_dir: &Path,
    env_vars: &[(String, String)],
    health_path: Option<&str>,
    drain_seconds: u32,
    domain: &str,
) -> DaemonResponse {
    // Start new instance alongside old
    let port = {
        let mut pm = process_manager.lock().await;
        match pm
            .start(app, release_dir, binary_name, app_type, data_dir, env_vars)
            .await
        {
            Ok(port) => port,
            Err(e) => {
                return DaemonResponse {
                    success: false,
                    message: format!("failed to start app: {e}"),
                    port: None,
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
    if let Err(resp) = run_health_check(process_manager, app, port, health_path).await {
        return resp;
    }

    // Swap: drain old instance, promote new
    {
        let mut pm = process_manager.lock().await;
        if let Err(e) = pm.swap(app, drain_seconds).await {
            tracing::warn!(app, err = %e, "swap warning (may be first deploy)");
        }
    }

    route_table.set(domain, port);
    tracing::info!(
        app,
        port,
        domain,
        strategy = "blue-green",
        "deploy activated"
    );

    DaemonResponse {
        success: true,
        message: format!("deployed {app} on port {port}"),
        port: Some(port),
    }
}

/// Sequential: stop old → start new → health check → activate.
/// Sub-second blip. Use for SQLite apps to avoid write contention.
async fn deploy_sequential(
    process_manager: &Arc<Mutex<ProcessManager>>,
    route_table: &RouteTable,
    app: &str,
    release_dir: &Path,
    binary_name: &str,
    app_type: AppType,
    data_dir: &Path,
    env_vars: &[(String, String)],
    health_path: Option<&str>,
    drain_seconds: u32,
    domain: &str,
) -> DaemonResponse {
    // Stop old instance first (removes route so no traffic during gap)
    {
        let mut pm = process_manager.lock().await;
        if pm.active_port(app).is_some() {
            route_table.remove(domain);
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
        let mut pm = process_manager.lock().await;
        match pm
            .start(app, release_dir, binary_name, app_type, data_dir, env_vars)
            .await
        {
            Ok(port) => port,
            Err(e) => {
                return DaemonResponse {
                    success: false,
                    message: format!("failed to start app: {e}"),
                    port: None,
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
    if let Err(resp) = run_health_check(process_manager, app, port, health_path).await {
        return resp;
    }

    // Promote pending to active (no swap needed — old was already stopped)
    {
        let mut pm = process_manager.lock().await;
        pm.promote_pending_to_active(app);
    }

    route_table.set(domain, port);
    tracing::info!(
        app,
        port,
        domain,
        strategy = "sequential",
        "deploy activated"
    );

    DaemonResponse {
        success: true,
        message: format!("deployed {app} on port {port}"),
        port: Some(port),
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
        },
        Err(e) => DaemonResponse {
            success: false,
            message: format!("failed to stop {app}: {e}"),
            port: None,
        },
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
    fn serialize_response() {
        let resp = DaemonResponse {
            success: true,
            message: "deployed myapp on port 10001".into(),
            port: Some(10001),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.port, Some(10001));
    }
}
