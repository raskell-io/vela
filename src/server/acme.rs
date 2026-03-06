use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use tokio_rustls::rustls;

/// Stores ACME HTTP-01 challenge tokens for the proxy to serve.
#[derive(Debug, Clone, Default)]
pub struct ChallengeStore {
    /// token → key_authorization
    tokens: Arc<RwLock<HashMap<String, String>>>,
}

impl ChallengeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, token: &str, key_auth: &str) {
        self.tokens
            .write()
            .expect("challenge store lock poisoned")
            .insert(token.to_string(), key_auth.to_string());
    }

    pub fn get(&self, token: &str) -> Option<String> {
        self.tokens
            .read()
            .expect("challenge store lock poisoned")
            .get(token)
            .cloned()
    }

    pub fn remove(&self, token: &str) {
        self.tokens
            .write()
            .expect("challenge store lock poisoned")
            .remove(token);
    }
}

/// Paths for stored TLS certificates.
pub struct CertPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl CertPaths {
    pub fn for_domain(data_dir: &Path, domain: &str) -> Self {
        let tls_dir = data_dir.join("tls");
        Self {
            cert: tls_dir.join(format!("{domain}.crt")),
            key: tls_dir.join(format!("{domain}.key")),
        }
    }

    pub fn exists(&self) -> bool {
        self.cert.exists() && self.key.exists()
    }
}

/// Check if any domains need certificate provisioning.
/// Returns domains that don't have valid certs yet.
pub fn domains_needing_certs(data_dir: &Path, domains: &[String]) -> Vec<String> {
    domains
        .iter()
        .filter(|d| !CertPaths::for_domain(data_dir, d).exists())
        .cloned()
        .collect()
}

/// ACME client for automatic certificate provisioning via Let's Encrypt.
pub struct AcmeClient {
    data_dir: PathBuf,
    challenge_store: ChallengeStore,
    /// Use Let's Encrypt staging for testing, production for real certs.
    use_staging: bool,
}

impl AcmeClient {
    pub fn new(data_dir: &Path, challenge_store: ChallengeStore, use_staging: bool) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            challenge_store,
            use_staging,
        }
    }

    /// Provision a certificate for the given domain using HTTP-01 challenge.
    /// Returns the paths to the saved cert and key files.
    pub async fn provision_cert(&self, domain: &str, contact_email: &str) -> Result<CertPaths> {
        use instant_acme::{
            Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder,
            OrderStatus,
        };

        let directory_url = if self.use_staging {
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        } else {
            "https://acme-v02.api.letsencrypt.org/directory"
        };

        tracing::info!(
            domain,
            staging = self.use_staging,
            "starting ACME cert provisioning"
        );

        // Create or load ACME account
        let contact = format!("mailto:{contact_email}");
        let (account, _credentials) = Account::create(
            &NewAccount {
                contact: &[&contact],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory_url,
            None,
        )
        .await
        .context("failed to create ACME account")?;

        // Create order for the domain
        let identifier = Identifier::Dns(domain.to_string());
        let mut order = account
            .new_order(&NewOrder {
                identifiers: &[identifier],
            })
            .await
            .context("failed to create ACME order")?;

        let state = order.state();
        tracing::debug!(domain, status = ?state.status, "ACME order created");

        // Get authorizations
        let authorizations = order
            .authorizations()
            .await
            .context("failed to get ACME authorizations")?;

        for auth in &authorizations {
            match auth.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                _ => anyhow::bail!("unexpected authorization status: {:?}", auth.status),
            }

            // Find the HTTP-01 challenge
            let challenge = auth
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
                .context("no HTTP-01 challenge found")?;

            // Set up the challenge response
            let key_auth = order.key_authorization(challenge);
            self.challenge_store
                .set(&challenge.token, key_auth.as_str());

            tracing::info!(domain, token = %challenge.token, "serving ACME challenge");

            // Tell the ACME server we're ready
            order
                .set_challenge_ready(&challenge.url)
                .await
                .context("failed to set challenge ready")?;
        }

        // Poll until the order is ready
        let mut tries = 0;
        let state = loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let state = order.refresh().await.context("failed to refresh order")?;
            tries += 1;

            match state.status {
                OrderStatus::Ready | OrderStatus::Valid => break state,
                OrderStatus::Pending if tries < 15 => continue,
                OrderStatus::Processing if tries < 15 => continue,
                status => {
                    // Clean up challenge tokens
                    for auth in &authorizations {
                        for c in &auth.challenges {
                            if c.r#type == ChallengeType::Http01 {
                                self.challenge_store.remove(&c.token);
                            }
                        }
                    }
                    anyhow::bail!("ACME order failed with status: {status:?}");
                }
            }
        };

        tracing::info!(domain, status = ?state.status, "ACME order ready");

        // Clean up challenge tokens
        for auth in &authorizations {
            for c in &auth.challenges {
                if c.r#type == ChallengeType::Http01 {
                    self.challenge_store.remove(&c.token);
                }
            }
        }

        // Generate a CSR and finalize the order
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
            .context("failed to create cert params")?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let private_key = rcgen::KeyPair::generate().context("failed to generate key pair")?;
        let csr = params
            .serialize_request(&private_key)
            .context("failed to serialize CSR")?;

        order
            .finalize(csr.der())
            .await
            .context("failed to finalize ACME order")?;

        // Wait for certificate
        let cert_chain_pem = loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match order
                .certificate()
                .await
                .context("failed to get certificate")?
            {
                Some(cert) => break cert,
                None => {
                    tries += 1;
                    if tries > 20 {
                        anyhow::bail!("timed out waiting for certificate");
                    }
                }
            }
        };

        // Save cert and key to disk
        let paths = CertPaths::for_domain(&self.data_dir, domain);
        let tls_dir = self.data_dir.join("tls");
        std::fs::create_dir_all(&tls_dir).context("failed to create TLS directory")?;

        std::fs::write(&paths.cert, cert_chain_pem.as_bytes())
            .context("failed to write certificate")?;
        std::fs::write(&paths.key, private_key.serialize_pem().as_bytes())
            .context("failed to write private key")?;

        // Restrict key file permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o600))
                .context("failed to set key permissions")?;
        }

        tracing::info!(
            domain,
            cert = %paths.cert.display(),
            "certificate provisioned successfully"
        );

        Ok(paths)
    }
}

/// Dynamic TLS certificate resolver that supports multiple domains.
/// Implements rustls `ResolvesServerCert` for SNI-based cert selection.
pub struct CertResolver {
    certs: Arc<RwLock<HashMap<String, Arc<rustls::sign::CertifiedKey>>>>,
}

impl CertResolver {
    pub fn new() -> Self {
        Self {
            certs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load a certificate for a domain from PEM files.
    pub fn load_cert(&self, domain: &str, cert_path: &Path, key_path: &Path) -> Result<()> {
        let cert_pem = std::fs::read(cert_path)
            .with_context(|| format!("failed to read cert: {}", cert_path.display()))?;
        let key_pem = std::fs::read(key_path)
            .with_context(|| format!("failed to read key: {}", key_path.display()))?;

        let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_pem[..])
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to parse certificates")?;

        let key = rustls_pemfile::private_key(&mut &key_pem[..])
            .context("failed to parse private key")?
            .context("no private key found")?;

        let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
            .map_err(|e| anyhow::anyhow!("unsupported key type: {e}"))?;

        let certified_key = rustls::sign::CertifiedKey::new(certs, signing_key);

        self.certs
            .write()
            .expect("cert resolver lock poisoned")
            .insert(domain.to_string(), Arc::new(certified_key));

        tracing::info!(domain, "loaded TLS certificate");
        Ok(())
    }

    /// Check if a certificate is loaded for a domain.
    pub fn has_cert(&self, domain: &str) -> bool {
        self.certs
            .read()
            .expect("cert resolver lock poisoned")
            .contains_key(domain)
    }
}

impl rustls::server::ResolvesServerCert for CertResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let server_name = client_hello.server_name()?;
        self.certs
            .read()
            .expect("cert resolver lock poisoned")
            .get(server_name)
            .cloned()
    }
}

/// Provision certificates for all domains that need them, then load into the resolver.
pub async fn provision_and_load_certs(
    data_dir: &Path,
    domains: &[String],
    contact_email: &str,
    challenge_store: &ChallengeStore,
    cert_resolver: &Arc<CertResolver>,
    use_staging: bool,
) -> Result<()> {
    // First load any existing certs
    for domain in domains {
        let paths = CertPaths::for_domain(data_dir, domain);
        if paths.exists() {
            if let Err(e) = cert_resolver.load_cert(domain, &paths.cert, &paths.key) {
                tracing::warn!(domain, err = %e, "failed to load existing cert, will re-provision");
            } else {
                continue;
            }
        }
    }

    // Provision missing certs
    let needed = domains_needing_certs(data_dir, domains);
    if needed.is_empty() {
        return Ok(());
    }

    let client = AcmeClient::new(data_dir, challenge_store.clone(), use_staging);

    for domain in &needed {
        // Skip cert resolver check — we already know these need provisioning
        match client.provision_cert(domain, contact_email).await {
            Ok(paths) => {
                if let Err(e) = cert_resolver.load_cert(domain, &paths.cert, &paths.key) {
                    tracing::error!(domain, err = %e, "failed to load provisioned cert");
                }
            }
            Err(e) => {
                tracing::error!(domain, err = %e, "failed to provision certificate");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_store_crud() {
        let store = ChallengeStore::new();
        assert!(store.get("token1").is_none());

        store.set("token1", "auth1");
        assert_eq!(store.get("token1"), Some("auth1".to_string()));

        store.remove("token1");
        assert!(store.get("token1").is_none());
    }

    #[test]
    fn cert_paths_for_domain() {
        let paths = CertPaths::for_domain(Path::new("/var/vela"), "example.com");
        assert_eq!(paths.cert, PathBuf::from("/var/vela/tls/example.com.crt"));
        assert_eq!(paths.key, PathBuf::from("/var/vela/tls/example.com.key"));
    }

    #[test]
    fn cert_resolver_empty() {
        let resolver = CertResolver::new();
        assert!(!resolver.has_cert("example.com"));
    }
}
