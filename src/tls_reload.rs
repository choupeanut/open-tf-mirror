use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use rustls::{
    crypto::aws_lc_rs::sign::any_supported_type,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

#[derive(Debug)]
pub struct ReloadingCertResolver {
    cert_path: PathBuf,
    key_path: PathBuf,
    last_good: RwLock<Option<Arc<CertifiedKey>>>,
}

impl ReloadingCertResolver {
    pub fn new(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let resolver = Self {
            cert_path: cert_path.as_ref().to_path_buf(),
            key_path: key_path.as_ref().to_path_buf(),
            last_good: RwLock::new(None),
        };
        let key = resolver.load_certified_key()?;
        *resolver.last_good.write() = Some(Arc::new(key));
        Ok(resolver)
    }

    pub fn resolve_current_cert(&self) -> Result<Arc<CertifiedKey>> {
        let key = Arc::new(self.load_certified_key()?);
        *self.last_good.write() = Some(Arc::clone(&key));
        Ok(key)
    }

    fn load_certified_key(&self) -> Result<CertifiedKey> {
        let cert_file = File::open(&self.cert_path)
            .with_context(|| format!("open TLS certificate {}", self.cert_path.display()))?;
        let mut cert_reader = BufReader::new(cert_file);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("parse TLS certificate {}", self.cert_path.display()))?;

        let key_file = File::open(&self.key_path)
            .with_context(|| format!("open TLS private key {}", self.key_path.display()))?;
        let mut key_reader = BufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut key_reader)
            .with_context(|| format!("parse TLS private key {}", self.key_path.display()))?
            .context("TLS private key file did not contain a supported key")?;

        build_certified_key(certs, key)
    }
}

impl ResolvesServerCert for ReloadingCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match self.resolve_current_cert() {
            Ok(key) => Some(key),
            Err(err) => {
                tracing::warn!(
                    cert_path = %self.cert_path.display(),
                    key_path = %self.key_path.display(),
                    error = %err,
                    "failed to reload TLS certificate"
                );
                None
            }
        }
    }
}

fn build_certified_key(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey> {
    let signing_key = any_supported_type(&key).context("unsupported TLS private key type")?;
    Ok(CertifiedKey::new(certs, signing_key))
}
