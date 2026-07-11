use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use rustls::{
    crypto::aws_lc_rs::sign::any_supported_type,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

#[derive(Debug)]
pub struct ReloadingCertResolver {
    cert_path: PathBuf,
    key_path: PathBuf,
    cached_key: RwLock<(Arc<CertifiedKey>, Instant)>,
    reload_interval: Duration,
}

impl ReloadingCertResolver {
    pub fn new(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        Self::new_with_reload_interval(cert_path, key_path, Duration::from_secs(5))
    }

    pub fn new_with_reload_interval(
        cert_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
        reload_interval: Duration,
    ) -> Result<Self> {
        let cert_path = cert_path.as_ref().to_path_buf();
        let key_path = key_path.as_ref().to_path_buf();
        let key = Arc::new(load_certified_key(&cert_path, &key_path)?);
        Ok(Self {
            cert_path,
            key_path,
            cached_key: RwLock::new((key, Instant::now())),
            reload_interval,
        })
    }

    pub fn resolve_current_cert(&self) -> Arc<CertifiedKey> {
        let now = Instant::now();
        {
            let cached = self.cached_key.read();
            if now.duration_since(cached.1) < self.reload_interval {
                return Arc::clone(&cached.0);
            }
        }

        match self.load_certified_key() {
            Ok(key) => {
                let key = Arc::new(key);
                *self.cached_key.write() = (Arc::clone(&key), now);
                key
            }
            Err(err) => {
                tracing::warn!(
                    cert_path = %self.cert_path.display(),
                    key_path = %self.key_path.display(),
                    error = %err,
                    "failed to reload TLS certificate, falling back to cached certificate"
                );
                let mut cached = self.cached_key.write();
                cached.1 = now;
                Arc::clone(&cached.0)
            }
        }
    }

    fn load_certified_key(&self) -> Result<CertifiedKey> {
        load_certified_key(&self.cert_path, &self.key_path)
    }
}

impl ResolvesServerCert for ReloadingCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.resolve_current_cert())
    }
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<CertifiedKey> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("open TLS certificate {}", cert_path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse TLS certificate {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("TLS certificate file did not contain a certificate chain");
    }

    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("parse TLS private key {}", key_path.display()))?;

    build_certified_key(certs, key)
}

fn build_certified_key(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey> {
    let signing_key = any_supported_type(&key).context("unsupported TLS private key type")?;
    let certified_key = CertifiedKey::new(certs, signing_key);
    certified_key
        .keys_match()
        .context("TLS certificate and private key do not match")?;
    Ok(certified_key)
}
