use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::acme::{CertResolver, ChallengeStore};
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

    #[allow(dead_code)]
    pub fn all(&self) -> HashMap<String, u16> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .clone()
    }
}

/// Start the reverse proxy.
///
/// The proxy listens on the configured HTTP (and optionally HTTPS) ports,
/// routes requests by Host header to upstream app ports via the RouteTable.
///
/// Returns a handle that keeps the proxy alive.
pub fn start_proxy(
    config: &ServerConfig,
    route_table: RouteTable,
    challenge_store: ChallengeStore,
    cert_resolver: Option<Arc<CertResolver>>,
) -> anyhow::Result<ProxyHandle> {
    let client_verifier = build_client_verifier(&config.tls)?;
    let http_port = config.proxy.http_port;
    let rt = route_table.clone();
    let cs = challenge_store.clone();

    // Shared HTTP client for upstream forwarding (connection pooling)
    let upstream_client = reqwest::Client::builder().no_proxy().build().unwrap();

    // Start HTTPS proxy if we have either static certs or a dynamic cert resolver
    let tls_config = config.tls.clone();
    let has_static_tls = tls_config.cert.is_some() && tls_config.key.is_some();
    let has_tls = has_static_tls || cert_resolver.is_some();

    let https_port = config.proxy.https_port;

    // HTTP proxy — also handles ACME challenges and HTTP→HTTPS redirects
    let client_http = upstream_client.clone();
    tokio::spawn(async move {
        if let Err(e) = run_http_proxy(http_port, rt, cs, client_http, has_tls, https_port).await {
            tracing::error!(err = %e, "HTTP proxy exited with error");
        }
    });

    if has_tls {
        let rt2 = route_table.clone();
        let client_https = upstream_client;

        if let Some(resolver) = cert_resolver {
            // Dynamic cert resolution (ACME or mixed)
            if let (Some(cert_path), Some(key_path)) = (&tls_config.cert, &tls_config.key)
                && let Err(e) = resolver.load_cert("*", cert_path, key_path)
            {
                tracing::warn!(err = %e, "failed to load static TLS cert into resolver");
            }

            let cv = client_verifier.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    run_https_proxy_dynamic(https_port, rt2, resolver, client_https, cv).await
                {
                    tracing::error!(err = %e, "HTTPS proxy exited with error");
                }
            });
        } else {
            let cert_path = tls_config.cert.clone().unwrap();
            let key_path = tls_config.key.clone().unwrap();

            let cv = client_verifier.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    run_https_proxy(https_port, rt2, &cert_path, &key_path, client_https, cv).await
                {
                    tracing::error!(err = %e, "HTTPS proxy exited with error");
                }
            });
        }

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type BoxBody = http_body_util::Full<hyper::body::Bytes>;

async fn run_http_proxy(
    port: u16,
    route_table: RouteTable,
    challenge_store: ChallengeStore,
    client: reqwest::Client,
    has_tls: bool,
    https_port: u16,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, "HTTP proxy listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        let rt = route_table.clone();
        let cs = challenge_store.clone();
        let client = client.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let rt = rt.clone();
                let cs = cs.clone();
                let client = client.clone();
                async move { handle_http_request(req, &rt, &cs, &client, has_tls, https_port).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
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
    client: reqwest::Client,
    client_verifier: Option<Arc<dyn tokio_rustls::rustls::server::danger::ClientCertVerifier>>,
) -> anyhow::Result<()> {
    use tokio_rustls::TlsAcceptor;

    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let builder = tokio_rustls::rustls::ServerConfig::builder();
    let tls_config = if let Some(verifier) = client_verifier {
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?
    };

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, "HTTPS proxy listening (static cert)");

    loop {
        let (stream, addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let rt = route_table.clone();
        let client = client.clone();

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
                let client = client.clone();
                async move { forward_request(req, &rt, &client).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .with_upgrades()
                .await
            {
                tracing::debug!(addr = %addr, err = %e, "connection error");
            }
        });
    }
}

async fn run_https_proxy_dynamic(
    port: u16,
    route_table: RouteTable,
    cert_resolver: Arc<CertResolver>,
    client: reqwest::Client,
    client_verifier: Option<Arc<dyn tokio_rustls::rustls::server::danger::ClientCertVerifier>>,
) -> anyhow::Result<()> {
    use tokio_rustls::TlsAcceptor;

    let builder = tokio_rustls::rustls::ServerConfig::builder();
    let tls_config = if let Some(verifier) = client_verifier {
        builder
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(cert_resolver)
    } else {
        builder
            .with_no_client_auth()
            .with_cert_resolver(cert_resolver)
    };

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, "HTTPS proxy listening (dynamic certs)");

    loop {
        let (stream, addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let rt = route_table.clone();
        let client = client.clone();

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
                let client = client.clone();
                async move { forward_request(req, &rt, &client).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .with_upgrades()
                .await
            {
                tracing::debug!(addr = %addr, err = %e, "connection error");
            }
        });
    }
}

/// Handle an HTTP request: serve ACME challenges, redirect to HTTPS if TLS is
/// configured, or forward to upstream.
async fn handle_http_request(
    req: Request<Incoming>,
    route_table: &RouteTable,
    challenge_store: &ChallengeStore,
    client: &reqwest::Client,
    has_tls: bool,
    https_port: u16,
) -> Result<Response<BoxBody>, hyper::Error> {
    // Always serve ACME HTTP-01 challenges over plain HTTP
    let path = req.uri().path().to_string();
    if path.starts_with("/.well-known/acme-challenge/") {
        let token = path.trim_start_matches("/.well-known/acme-challenge/");
        if let Some(key_auth) = challenge_store.get(token) {
            tracing::info!(token, "serving ACME challenge");
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(BoxBody::new(hyper::body::Bytes::from(key_auth)))
                .unwrap());
        }
    }

    // If TLS is configured, redirect HTTP → HTTPS
    if has_tls {
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h))
            .unwrap_or_default();

        if !host.is_empty() {
            let location = if https_port == 443 {
                format!("https://{host}{path}")
            } else {
                format!("https://{host}:{https_port}{path}")
            };

            return Ok(Response::builder()
                .status(StatusCode::MOVED_PERMANENTLY)
                .header("location", location)
                .body(BoxBody::new(hyper::body::Bytes::from(
                    "301 Moved Permanently\n",
                )))
                .unwrap());
        }
    }

    // No TLS — forward directly
    forward_request(req, route_table, client).await
}

/// Check if a request is a WebSocket upgrade.
fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Handle a WebSocket upgrade by tunneling to the upstream app.
async fn handle_websocket_upgrade(
    req: Request<Incoming>,
    upstream_port: u16,
) -> Result<Response<BoxBody>, hyper::Error> {
    use hyper::body::Bytes;

    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();
    let headers = req.headers().clone();

    // Connect to upstream
    let mut upstream = match TcpStream::connect(format!("127.0.0.1:{upstream_port}")).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(upstream_port, err = %e, "websocket upstream connect failed");
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(BoxBody::new(Bytes::from("502 Bad Gateway\n")))
                .unwrap());
        }
    };

    // Build raw HTTP upgrade request for upstream
    let mut raw_req = format!("GET {path} HTTP/1.1\r\n");
    for (name, value) in &headers {
        if let Ok(v) = value.to_str() {
            raw_req.push_str(&format!("{}: {v}\r\n", name.as_str()));
        }
    }
    raw_req.push_str("\r\n");

    if let Err(e) = upstream.write_all(raw_req.as_bytes()).await {
        tracing::warn!(err = %e, "websocket upstream write failed");
        return Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(BoxBody::new(Bytes::from("502 Bad Gateway\n")))
            .unwrap());
    }

    // Read upstream response headers (until \r\n\r\n)
    let mut buf = vec![0u8; 4096];
    let mut total = 0;
    let header_end;
    loop {
        match upstream.read(&mut buf[total..]).await {
            Ok(0) => {
                tracing::warn!("websocket upstream closed before response");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(BoxBody::new(Bytes::from("502 Bad Gateway\n")))
                    .unwrap());
            }
            Ok(n) => {
                total += n;
                if let Some(pos) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
                if total >= buf.len() {
                    buf.resize(total + 4096, 0);
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "websocket upstream read failed");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(BoxBody::new(Bytes::from("502 Bad Gateway\n")))
                    .unwrap());
            }
        }
    }

    // Parse upstream response status and headers
    let header_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_str.split("\r\n");

    let status_line = lines.next().unwrap_or("");
    let status_code = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502);

    if status_code != 101 {
        tracing::warn!(status_code, "websocket upstream rejected upgrade");
        return Ok(Response::builder()
            .status(StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY))
            .body(BoxBody::new(Bytes::from("upstream rejected websocket\n")))
            .unwrap());
    }

    // Build 101 response for client, forwarding upstream headers
    let mut resp = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            resp = resp.header(name.trim(), value.trim());
        }
    }

    // Any bytes after the headers are early WebSocket frames
    let remaining = buf[header_end..total].to_vec();

    // Spawn task to bridge client ↔ upstream after hyper completes the upgrade
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut client = TokioIo::new(upgraded);

                // Forward any early upstream data to client
                if !remaining.is_empty() {
                    if let Err(e) = client.write_all(&remaining).await {
                        tracing::debug!(err = %e, "websocket: failed to write remaining data");
                        return;
                    }
                }

                match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                    Ok((to_upstream, to_client)) => {
                        tracing::debug!(to_upstream, to_client, "websocket tunnel closed");
                    }
                    Err(e) => {
                        tracing::debug!(err = %e, "websocket tunnel ended");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "websocket upgrade failed");
            }
        }
    });

    Ok(resp.body(BoxBody::new(Bytes::new())).unwrap())
}

/// Forward a request to the upstream app based on the Host header.
async fn forward_request(
    req: Request<Incoming>,
    route_table: &RouteTable,
    client: &reqwest::Client,
) -> Result<Response<BoxBody>, hyper::Error> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();

    let upstream_port = match route_table.get(&host) {
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

    // WebSocket upgrade — tunnel directly instead of using reqwest
    if is_websocket_upgrade(&req) {
        tracing::debug!(host, upstream_port, "websocket upgrade");
        return handle_websocket_upgrade(req, upstream_port).await;
    }

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

    let (parts, body) = req.into_parts();

    // Forward headers (except Host)
    for (key, value) in &parts.headers {
        if key != hyper::header::HOST
            && let Ok(v) = value.to_str()
        {
            upstream_req = upstream_req.header(key.as_str(), v);
        }
    }

    // Forward body
    let body_bytes = match http_body_util::BodyExt::collect(body).await {
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
                .body(BoxBody::new(hyper::body::Bytes::from(
                    "502 Bad Gateway: upstream error\n",
                )))
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

/// Build an optional client certificate verifier from the TLS config.
/// When `client_ca` is set, returns a verifier that requires valid client certs
/// signed by the specified CA (e.g. Cloudflare Authenticated Origin Pulls).
fn build_client_verifier(
    tls_config: &crate::config::TlsConfig,
) -> anyhow::Result<Option<Arc<dyn tokio_rustls::rustls::server::danger::ClientCertVerifier>>> {
    let ca_path = match &tls_config.client_ca {
        Some(path) => path,
        None => return Ok(None),
    };

    let ca_certs = load_certs(ca_path)?;
    let mut root_store = tokio_rustls::rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store.add(cert)?;
    }

    let verifier =
        tokio_rustls::rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build client cert verifier: {e}"))?;

    tracing::info!(ca = %ca_path.display(), "client certificate verification enabled");
    Ok(Some(verifier))
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
