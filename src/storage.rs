use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use parking_lot::{Mutex as SyncMutex, RwLock};
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore},
};

use crate::{
    metadata::PlatformMetadata,
    outbound::{OutboundClient, OutboundError},
    provider::ArchiveName,
};

const MAX_PROVIDER_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONCURRENT_DOWNLOADS: usize = 8;
const MAX_VERIFIED_ENTRIES: usize = 16384;
const HASH_BUFFER_BYTES: usize = 256 * 1024;
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
    #[error("provider outbound policy failed: {0}")]
    Outbound(#[from] OutboundError),
    #[error("provider cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Identity of a file as observed from its metadata. Archives are only
/// published by atomic rename, so any content replacement changes this.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        }
    }
}

#[derive(Debug, Clone)]
struct VerifiedFile {
    identity: FileIdentity,
    checksum: String,
}

#[derive(Debug, Clone)]
pub struct ProviderStorage {
    root: Arc<PathBuf>,
    bundled_mirror: Option<Arc<PathBuf>>,
    locks: Arc<RwLock<HashMap<ProviderArchiveKey, Arc<Mutex<()>>>>>,
    max_archive_bytes: u64,
    outbound: OutboundClient,
    download_limit: Arc<Semaphore>,
    verified: Arc<SyncMutex<HashMap<PathBuf, VerifiedFile>>>,
}

impl ProviderStorage {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let bundled = std::env::var_os("TF_PLUGIN_MIRROR_DIR").map(PathBuf::from);
        Self::build(
            root.as_ref(),
            bundled.as_deref(),
            MAX_PROVIDER_ARCHIVE_BYTES,
            OutboundClient::new(crate::outbound::OutboundMode::Direct, None)
                .expect("provider download client configuration must be valid"),
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
            OutboundClient::new(crate::outbound::OutboundMode::Direct, None)
                .map_err(|error| ProviderStorageError::InvalidKey(error.to_string()))?,
        )
    }

    pub fn with_bundled_mirror_and_outbound(
        root: impl AsRef<Path>,
        bundled_mirror: Option<impl AsRef<Path>>,
        outbound: OutboundClient,
    ) -> Result<Self, ProviderStorageError> {
        Self::build(
            root.as_ref(),
            bundled_mirror.as_ref().map(|path| path.as_ref()),
            MAX_PROVIDER_ARCHIVE_BYTES,
            outbound,
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
            OutboundClient::for_tests(),
        )
    }

    fn build(
        root: &Path,
        bundled_mirror: Option<&Path>,
        max_archive_bytes: u64,
        outbound: OutboundClient,
    ) -> Result<Self, ProviderStorageError> {
        Ok(Self {
            root: Arc::new(root.to_path_buf()),
            bundled_mirror: bundled_mirror.map(|path| Arc::new(path.to_path_buf())),
            locks: Arc::default(),
            // The PVC capacity is the aggregate persistent-cache quota; this is only a per-archive guard.
            max_archive_bytes,
            outbound,
            download_limit: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
            verified: Arc::default(),
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
        let expected = expected_checksum(metadata)?;

        if let Some(path) = self.bundled_path(key)
            && self.archive_matches_checksum(&path, &expected).await?
        {
            return Ok(path);
        }
        let destination = self.archive_path(key);
        if self
            .archive_matches_checksum(&destination, &expected)
            .await?
        {
            return Ok(destination);
        }

        // The download runs in a detached task so a disconnecting client does
        // not cancel it; later waiters then see the cache hit.
        let storage = self.clone();
        let key = key.clone();
        let metadata = metadata.clone();
        tokio::spawn(async move {
            storage
                .locked_load_or_fetch(&key, &metadata, &expected, &destination)
                .await
        })
        .await
        .map_err(|error| ProviderStorageError::Io(std::io::Error::other(error)))?
    }

    async fn locked_load_or_fetch(
        &self,
        key: &ProviderArchiveKey,
        metadata: &PlatformMetadata,
        expected: &str,
        destination: &Path,
    ) -> Result<PathBuf, ProviderStorageError> {
        let lock = self.lock_for(key);
        let _guard = lock.mutex.lock().await;
        if let Some(path) = self.bundled_path(key)
            && self.archive_matches_checksum(&path, expected).await?
        {
            return Ok(path);
        }
        if self.archive_matches_checksum(destination, expected).await? {
            return Ok(destination.to_path_buf());
        }

        self.download(key, metadata, expected, destination).await?;
        Ok(destination.to_path_buf())
    }

    #[doc(hidden)]
    pub fn verified_entry_count(&self) -> usize {
        self.verified.lock().len()
    }

    fn record_verified(&self, path: &Path, identity: FileIdentity, checksum: &str) {
        let mut verified = self.verified.lock();
        if verified.len() >= MAX_VERIFIED_ENTRIES && !verified.contains_key(path) {
            verified.clear();
        }
        verified.insert(
            path.to_path_buf(),
            VerifiedFile {
                identity,
                checksum: checksum.to_string(),
            },
        );
    }

    async fn archive_matches_checksum(
        &self,
        path: &Path,
        expected: &str,
    ) -> Result<bool, ProviderStorageError> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > self.max_archive_bytes {
            return Ok(false);
        }
        let identity = FileIdentity::from_metadata(&metadata);
        if self
            .verified
            .lock()
            .get(path)
            .is_some_and(|entry| entry.identity == identity && entry.checksum == expected)
        {
            return Ok(true);
        }

        let owned_path = path.to_path_buf();
        let max = self.max_archive_bytes;
        let hashed = tokio::task::spawn_blocking(move || hash_file(&owned_path, max))
            .await
            .map_err(std::io::Error::other)??;
        match hashed {
            Some((actual, identity)) if actual == expected => {
                self.record_verified(path, identity, expected);
                Ok(true)
            }
            _ => {
                self.verified.lock().remove(path);
                Ok(false)
            }
        }
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
        expected: &str,
        destination: &Path,
    ) -> Result<(), ProviderStorageError> {
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

        let response = self
            .outbound
            .get(url, Duration::from_secs(15 * 60))
            .await?
            .error_for_status()?;
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
            return Err(ProviderStorageError::ChecksumMismatch {
                expected: expected.to_string(),
                actual,
            });
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp_path, destination).await?;
        cleanup.disarm();
        sync_parent(parent).await?;
        if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await
            && metadata.file_type().is_file()
        {
            self.record_verified(
                destination,
                FileIdentity::from_metadata(&metadata),
                expected,
            );
        }
        Ok(())
    }
}

/// Hashes a file, returning `None` when it is missing, not regular or too
/// large. The identity is taken from the opened file so it describes the
/// bytes that were hashed.
fn hash_file(path: &Path, max_bytes: u64) -> std::io::Result<Option<(String, FileIdentity)>> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Ok(None);
    }
    let identity = FileIdentity::from_metadata(&metadata);
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Ok(None);
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some((hex::encode(hasher.finalize()), identity)))
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

fn expected_checksum(metadata: &PlatformMetadata) -> Result<String, ProviderStorageError> {
    metadata
        .shasum
        .as_deref()
        .filter(|checksum| {
            checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
        .ok_or(ProviderStorageError::MissingChecksum)
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

/// Remove `.*.tmp` files left behind by a crash during an archive or metadata
/// write. Only call this before serving: a running download owns its temp file.
pub fn remove_stale_temp_files(data_dir: &Path) -> std::io::Result<usize> {
    fn walk(directory: &Path, removed: &mut usize) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(&entry.path(), removed)?;
            } else if file_type.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') && name.ends_with(".tmp") {
                    std::fs::remove_file(entry.path())?;
                    *removed += 1;
                }
            }
        }
        Ok(())
    }

    let mut removed = 0;
    for directory in ["providers", "metadata"] {
        walk(&data_dir.join(directory), &mut removed)?;
    }
    Ok(removed)
}
