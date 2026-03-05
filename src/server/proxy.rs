use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::config::ServerConfig;

/// Routing table: domain → upstream port.
/// Shared between the proxy and the deploy/process manager.
#[derive(Debug, Clone)]
pub struct RouteTable {
    routes: Arc<RwLock<HashMap<String, u16>>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn set(&self, domain: &str, port: u16) {
        self.routes
            .write()
            .expect("route table lock poisoned")
            .insert(domain.to_string(), port);
    }

    pub fn remove(&self, domain: &str) {
        self.routes
            .write()
            .expect("route table lock poisoned")
            .remove(domain);
    }

    pub fn get(&self, domain: &str) -> Option<u16> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .get(domain)
            .copied()
    }

    pub fn all(&self) -> HashMap<String, u16> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .clone()
    }
}

/// Start the Pingora-based reverse proxy.
///
/// The proxy listens on the configured HTTP (and optionally HTTPS) ports,
/// routes requests by Host header to upstream app ports via the RouteTable.
///
/// Returns a handle that keeps the proxy alive.
pub fn start_proxy(config: &ServerConfig, route_table: RouteTable) -> anyhow::Result<ProxyHandle> {
    // Pingora's Server requires running in the main thread context.
    // For now, we start a simple hyper-based reverse proxy that does
    // Host-header routing. This avoids Pingora's threading model issues
    // with tokio and keeps things simple for v1.
    //
    // The proxy runs as a tokio task inside the existing runtime.

    let http_port = config.proxy.http_port;
    let tls_config = config.tls.clone();
    let rt = route_table.clone();

    tokio::spawn(async move {
        if let Err(e) = run_http_proxy(http_port, rt).await {
            tracing::error!(err = %e, "HTTP proxy exited with error");
        }
    });

    if tls_config.cert.is_some() && tls_config.key.is_some() {
        let https_port = config.proxy.https_port;
        let rt2 = route_table.clone();
        let cert_path = tls_config.cert.clone().unwrap();
        let key_path = tls_config.key.clone().unwrap();

        tokio::spawn(async move {
            if let Err(e) = run_https_proxy(https_port, rt2, &cert_path, &key_path).await {
                tracing::error!(err = %e, "HTTPS proxy exited with error");
            }
        });

        tracing::info!(http_port, https_port, "proxy started (HTTP + HTTPS)");
    } else {
        tracing::info!(http_port, "proxy started (HTTP only, no TLS configured)");
    }

    Ok(ProxyHandle {
        _route_table: route_table,
    })
}

/// Handle that keeps proxy references alive.
pub struct ProxyHandle {
    _route_table: RouteTable,
}

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

type BoxBody = http_body_util::Full<hyper::body::Bytes>;

async fn run_http_proxy(port: u16, route_table: RouteTable) -> anyhow::Result<()> {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, "HTTP proxy listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        let rt = route_table.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let rt = rt.clone();
                async move { handle_request(req, &rt).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(addr = %addr, err = %e, "connection error");
            }
        });
    }
}

async fn run_https_proxy(
    port: u16,
    route_table: RouteTable,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> anyhow::Result<()> {
    use tokio_rustls::TlsAcceptor;

    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let mut tls_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?;

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, "HTTPS proxy listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let rt = route_table.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(addr = %addr, err = %e, "TLS handshake failed");
                    return;
                }
            };

            let service = service_fn(move |req: Request<Incoming>| {
                let rt = rt.clone();
                async move { handle_request(req, &rt).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .await
            {
                tracing::debug!(addr = %addr, err = %e, "connection error");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    route_table: &RouteTable,
) -> Result<Response<BoxBody>, hyper::Error> {
    // Extract host from Host header
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h))
        .unwrap_or("");

    let upstream_port = match route_table.get(host) {
        Some(port) => port,
        None => {
            tracing::debug!(host, "no route found");
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(BoxBody::new(hyper::body::Bytes::from(
                    "502 Bad Gateway: no app configured for this domain\n",
                )))
                .unwrap();
            return Ok(resp);
        }
    };

    // Forward the request to the upstream app
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    let uri = format!(
        "http://127.0.0.1:{upstream_port}{}",
        req.uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );

    let method = match req.method().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };

    let mut upstream_req = client.request(method, &uri);

    // Forward headers (except Host, which we set to the upstream)
    for (key, value) in req.headers() {
        if key != hyper::header::HOST {
            if let Ok(v) = value.to_str() {
                upstream_req = upstream_req.header(key.as_str(), v);
            }
        }
    }

    // Forward body
    let body_bytes = match http_body_util::BodyExt::collect(req.into_body()).await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => hyper::body::Bytes::new(),
    };
    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes.to_vec());
    }

    match upstream_req.send().await {
        Ok(upstream_resp) => {
            let status = StatusCode::from_u16(upstream_resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

            let mut resp_builder = Response::builder().status(status);
            for (key, value) in upstream_resp.headers() {
                resp_builder = resp_builder.header(key.as_str(), value.as_bytes());
            }

            let body = upstream_resp.bytes().await.unwrap_or_default();
            Ok(resp_builder.body(BoxBody::new(body)).unwrap())
        }
        Err(e) => {
            tracing::warn!(host, upstream_port, err = %e, "upstream request failed");
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(BoxBody::new(hyper::body::Bytes::from(format!(
                    "502 Bad Gateway: upstream error\n"
                ))))
                .unwrap();
            Ok(resp)
        }
    }
}

fn load_certs(
    path: &std::path::Path,
) -> anyhow::Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>> {
    let cert_file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open cert {}: {e}", path.display()))?;
    let mut reader = std::io::BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse cert: {e}"))?;
    Ok(certs)
}

fn load_private_key(
    path: &std::path::Path,
) -> anyhow::Result<tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>> {
    let key_file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open key {}: {e}", path.display()))?;
    let mut reader = std::io::BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| anyhow::anyhow!("failed to parse key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_crud() {
        let rt = RouteTable::new();

        rt.set("cyanea.bio", 10001);
        rt.set("archipelag.io", 10002);

        assert_eq!(rt.get("cyanea.bio"), Some(10001));
        assert_eq!(rt.get("archipelag.io"), Some(10002));
        assert_eq!(rt.get("unknown.com"), None);

        rt.set("cyanea.bio", 10003);
        assert_eq!(rt.get("cyanea.bio"), Some(10003));

        rt.remove("cyanea.bio");
        assert_eq!(rt.get("cyanea.bio"), None);

        let all = rt.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all["archipelag.io"], 10002);
    }
}
