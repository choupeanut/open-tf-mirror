use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use parking_lot::RwLock;
use reqwest::{Client, StatusCode, Url, header};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    net::lookup_host,
    sync::{Mutex, Semaphore},
};

use crate::{metadata::PlatformMetadata, provider::ArchiveName};

const MAX_PROVIDER_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONCURRENT_DOWNLOADS: usize = 8;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderArchiveKey {
    pub hostname: String,
    pub namespace: String,
    pub provider_type: String,
    pub filename: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderStorageError {
    #[error("invalid provider archive key: {0}")]
    InvalidKey(String),
    #[error("provider archive has no SHA-256 checksum")]
    MissingChecksum,
    #[error("provider archive checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("provider archive exceeds maximum size")]
    TooLarge,
    #[error("provider download URL violates policy: {0}")]
    Policy(String),
    #[error("provider redirect is invalid: {0}")]
    Redirect(String),
    #[error("provider download failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("provider cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ProviderStorage {
    root: Arc<PathBuf>,
    bundled_mirror: Option<Arc<PathBuf>>,
    locks: Arc<RwLock<HashMap<ProviderArchiveKey, Arc<Mutex<()>>>>>,
    max_archive_bytes: u64,
    download_policy: DownloadPolicy,
    download_limit: Arc<Semaphore>,
}

#[derive(Debug, Clone, Copy)]
struct DownloadPolicy {
    allow_http: bool,
    allow_non_public_ips: bool,
    max_redirects: usize,
}

impl DownloadPolicy {
    const PRODUCTION: Self = Self {
        allow_http: false,
        allow_non_public_ips: false,
        max_redirects: 5,
    };
    const TEST: Self = Self {
        allow_http: true,
        allow_non_public_ips: true,
        max_redirects: 5,
    };
}

impl ProviderStorage {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let bundled = std::env::var_os("TF_PLUGIN_MIRROR_DIR").map(PathBuf::from);
        Self::build(
            root.as_ref(),
            bundled.as_deref(),
            MAX_PROVIDER_ARCHIVE_BYTES,
            DownloadPolicy::PRODUCTION,
        )
        .expect("provider download client configuration must be valid")
    }

    pub fn with_bundled_mirror(
        root: impl AsRef<Path>,
        bundled_mirror: Option<impl AsRef<Path>>,
    ) -> Result<Self, ProviderStorageError> {
        Self::build(
            root.as_ref(),
            bundled_mirror.as_ref().map(|path| path.as_ref()),
            MAX_PROVIDER_ARCHIVE_BYTES,
            DownloadPolicy::PRODUCTION,
        )
    }

    pub fn new_for_tests(
        root: impl AsRef<Path>,
        bundled_mirror: Option<impl AsRef<Path>>,
        max_archive_bytes: u64,
    ) -> Result<Self, ProviderStorageError> {
        Self::build(
            root.as_ref(),
            bundled_mirror.as_ref().map(|path| path.as_ref()),
            max_archive_bytes,
            DownloadPolicy::TEST,
        )
    }

    fn build(
        root: &Path,
        bundled_mirror: Option<&Path>,
        max_archive_bytes: u64,
        download_policy: DownloadPolicy,
    ) -> Result<Self, ProviderStorageError> {
        Ok(Self {
            root: Arc::new(root.to_path_buf()),
            bundled_mirror: bundled_mirror.map(|path| Arc::new(path.to_path_buf())),
            locks: Arc::default(),
            // The PVC capacity is the aggregate persistent-cache quota; this is only a per-archive guard.
            max_archive_bytes,
            download_policy,
            download_limit: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
        })
    }

    pub fn archive_path(&self, key: &ProviderArchiveKey) -> PathBuf {
        self.root
            .join("providers")
            .join(&key.hostname)
            .join(&key.namespace)
            .join(&key.provider_type)
            .join(&key.filename)
    }

    pub async fn load_or_fetch(
        &self,
        key: &ProviderArchiveKey,
        metadata: &PlatformMetadata,
    ) -> Result<PathBuf, ProviderStorageError> {
        validate_key(key)?;
        if metadata.filename != key.filename {
            return Err(ProviderStorageError::InvalidKey(
                "metadata filename does not match request".into(),
            ));
        }

        if let Some(path) = self.bundled_path(key)
            && tokio::fs::metadata(&path).await.is_ok()
        {
            return Ok(path);
        }
        let destination = self.archive_path(key);
        if tokio::fs::metadata(&destination).await.is_ok() {
            return Ok(destination);
        }

        let lock = self.lock_for(key);
        let _guard = lock.mutex.lock().await;
        if let Some(path) = self.bundled_path(key)
            && tokio::fs::metadata(&path).await.is_ok()
        {
            return Ok(path);
        }
        if tokio::fs::metadata(&destination).await.is_ok() {
            return Ok(destination);
        }

        self.download(key, metadata, &destination).await?;
        Ok(destination)
    }

    fn bundled_path(&self, key: &ProviderArchiveKey) -> Option<PathBuf> {
        self.bundled_mirror.as_ref().map(|root| {
            root.join(&key.hostname)
                .join(&key.namespace)
                .join(&key.provider_type)
                .join(&key.filename)
        })
    }

    fn lock_for(&self, key: &ProviderArchiveKey) -> ManagedArchiveLock {
        let mutex = self
            .locks
            .write()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        ManagedArchiveLock {
            locks: Arc::clone(&self.locks),
            key: key.clone(),
            mutex,
        }
    }

    pub fn active_lock_count(&self) -> usize {
        self.locks.read().len()
    }

    pub fn available_download_permits(&self) -> usize {
        self.download_limit.available_permits()
    }

    async fn download(
        &self,
        key: &ProviderArchiveKey,
        metadata: &PlatformMetadata,
        destination: &Path,
    ) -> Result<(), ProviderStorageError> {
        let expected = metadata
            .shasum
            .as_deref()
            .filter(|checksum| {
                checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or(ProviderStorageError::MissingChecksum)?
            .to_ascii_lowercase();
        let url = Url::parse(&metadata.download_url)
            .map_err(|_| ProviderStorageError::InvalidKey("download URL".into()))?;
        let _permit = self
            .download_limit
            .acquire()
            .await
            .expect("provider download semaphore is never closed");
        let parent = destination
            .parent()
            .expect("provider archive paths have a parent");
        tokio::fs::create_dir_all(parent).await?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}-{}-{sequence}.tmp",
            key.filename,
            std::process::id()
        ));
        let mut cleanup = TempCleanup::new(temp_path.clone());
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options.open(&temp_path).await?;

        let response = self.send_with_policy(url).await?;
        if response
            .content_length()
            .is_some_and(|length| length > self.max_archive_bytes)
        {
            return Err(ProviderStorageError::TooLarge);
        }
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total = total.saturating_add(chunk.len() as u64);
            if total > self.max_archive_bytes {
                return Err(ProviderStorageError::TooLarge);
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            return Err(ProviderStorageError::ChecksumMismatch { expected, actual });
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp_path, destination).await?;
        cleanup.disarm();
        sync_parent(parent).await?;
        Ok(())
    }

    async fn send_with_policy(
        &self,
        mut url: Url,
    ) -> Result<reqwest::Response, ProviderStorageError> {
        for redirect_count in 0..=self.download_policy.max_redirects {
            let client = self.client_for_url(&url).await?;
            let response = client.get(url.clone()).send().await?;
            if !is_redirect(response.status()) {
                return Ok(response.error_for_status()?);
            }
            if redirect_count == self.download_policy.max_redirects {
                return Err(ProviderStorageError::Redirect(
                    "maximum redirect count exceeded".into(),
                ));
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .ok_or_else(|| ProviderStorageError::Redirect("missing Location header".into()))?
                .to_str()
                .map_err(|_| ProviderStorageError::Redirect("invalid Location header".into()))?;
            url = url
                .join(location)
                .map_err(|_| ProviderStorageError::Redirect("invalid Location URL".into()))?;
        }
        unreachable!("bounded redirect loop returns")
    }

    async fn client_for_url(&self, url: &Url) -> Result<Client, ProviderStorageError> {
        if url.scheme() != "https" && !(self.download_policy.allow_http && url.scheme() == "http") {
            return Err(ProviderStorageError::Policy("HTTPS is required".into()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ProviderStorageError::Policy("URL host is required".into()))?;
        let resolver_host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProviderStorageError::Policy("URL port is required".into()))?;
        let addresses = lookup_host((resolver_host, port))
            .await?
            .collect::<Vec<SocketAddr>>();
        if addresses.is_empty() {
            return Err(ProviderStorageError::Policy(
                "URL host did not resolve".into(),
            ));
        }
        if !self.download_policy.allow_non_public_ips
            && addresses.iter().any(|address| !is_public_ip(address.ip()))
        {
            return Err(ProviderStorageError::Policy(
                "URL resolves to a non-public address".into(),
            ));
        }
        Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15 * 60))
            .redirect(reqwest::redirect::Policy::none())
            // Pin the addresses that passed policy so DNS cannot change before connect.
            .resolve_to_addrs(resolver_host, &addresses)
            .build()
            .map_err(Into::into)
    }
}

struct ManagedArchiveLock {
    locks: Arc<RwLock<HashMap<ProviderArchiveKey, Arc<Mutex<()>>>>>,
    key: ProviderArchiveKey,
    mutex: Arc<Mutex<()>>,
}

impl Drop for ManagedArchiveLock {
    fn drop(&mut self) {
        let mut locks = self.locks.write();
        if Arc::strong_count(&self.mutex) == 2
            && locks
                .get(&self.key)
                .is_some_and(|mutex| Arc::ptr_eq(mutex, &self.mutex))
        {
            locks.remove(&self.key);
        }
    }
}

struct TempCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_key(key: &ProviderArchiveKey) -> Result<(), ProviderStorageError> {
    if !valid_hostname(&key.hostname)
        || !valid_component(&key.namespace)
        || !valid_component(&key.provider_type)
        || key.filename.contains(['/', '\\'])
        || ArchiveName::parse(&key.provider_type, &key.filename).is_err()
    {
        return Err(ProviderStorageError::InvalidKey(
            "invalid path component".into(),
        ));
    }
    Ok(())
}

fn valid_hostname(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(valid_component)
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, third, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && first != 0
                && first < 224
                && !(first == 100 && (64..=127).contains(&second))
                && !(first == 192 && second == 0)
                && !(first == 192 && second == 88 && third == 99)
                && !(first == 192 && second == 0 && third == 2)
                && !(first == 198 && matches!(second, 18 | 19))
                && !(first == 198 && second == 51 && third == 100)
                && !(first == 203 && second == 0 && third == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(ipv4) = ip.to_ipv4() {
                return is_public_ip(IpAddr::V4(ipv4));
            }
            let segments = ip.segments();
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unique_local()
                && !ip.is_unicast_link_local()
                && (segments[0] & 0xffc0) != 0xfec0
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

#[cfg(unix)]
async fn sync_parent(parent: &Path) -> Result<(), ProviderStorageError> {
    let parent = parent.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(std::io::Error::other)??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_parent(_parent: &Path) -> Result<(), ProviderStorageError> {
    Ok(())
}
