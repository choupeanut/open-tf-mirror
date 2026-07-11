use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream};
use parking_lot::RwLock;
use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use crate::{
    provider::ArchiveName,
    registry::{RegistryClient, RegistryError, RegistryVersion},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
type KeyLockMap<K> = Arc<RwLock<HashMap<K, Arc<Mutex<()>>>>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformMetadata {
    pub os: String,
    pub arch: String,
    pub filename: String,
    pub shasum: Option<String>,
    pub download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMetadata {
    pub hostname: String,
    pub namespace: String,
    pub provider_type: String,
    pub version: String,
    pub platforms: Vec<PlatformMetadata>,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("provider metadata was not found")]
    NotFound,
    #[error("invalid provider address: {0}")]
    InvalidAddress(String),
    #[error("provider registry is not allowed: {0}")]
    RegistryNotAllowed(String),
    #[error("provider synchronization is already running")]
    SyncInProgress,
    #[error("provider synchronization failed for {0} provider(s)")]
    SyncFailed(usize),
    #[error("registry error: {0}")]
    Registry(#[from] RegistryError),
    #[error("metadata I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid persisted metadata: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct ProviderKey {
    hostname: String,
    namespace: String,
    provider_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VersionKey {
    provider: ProviderKey,
    version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry<T> {
    fetched_at: u64,
    value: T,
}

impl<T> CacheEntry<T> {
    fn fresh(&self, freshness: Duration) -> bool {
        now_epoch().saturating_sub(self.fetched_at) < freshness.as_secs()
    }
}

#[derive(Debug, Clone)]
pub struct ProviderMetadataService {
    data_dir: Arc<PathBuf>,
    allowed_registries: Arc<HashSet<String>>,
    freshness: Duration,
    registry: RegistryClient,
    indices: Arc<RwLock<HashMap<ProviderKey, CacheEntry<Vec<RegistryVersion>>>>>,
    versions: Arc<RwLock<HashMap<VersionKey, CacheEntry<VersionMetadata>>>>,
    index_locks: KeyLockMap<ProviderKey>,
    version_locks: KeyLockMap<VersionKey>,
    syncing: Arc<AtomicBool>,
}

pub type ProviderMetadataStore = ProviderMetadataService;

impl Default for ProviderMetadataService {
    fn default() -> Self {
        Self::new(
            ".",
            HashSet::from(["registry.terraform.io".to_string()]),
            Duration::from_secs(30 * 60),
        )
        .expect("default registry client must be valid")
    }
}

impl ProviderMetadataService {
    pub fn new<I, S>(
        data_dir: impl AsRef<Path>,
        allowed_registries: I,
        freshness: Duration,
    ) -> Result<Self, MetadataError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_registry_client(
            data_dir,
            allowed_registries,
            freshness,
            RegistryClient::new()?,
        )
    }

    pub fn with_registry_client<I, S>(
        data_dir: impl AsRef<Path>,
        allowed_registries: I,
        freshness: Duration,
        registry: RegistryClient,
    ) -> Result<Self, MetadataError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allowed_registries = allowed_registries
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<_>>();
        for hostname in &allowed_registries {
            validate_hostname(hostname)?;
        }
        let metadata_dir = data_dir.as_ref().join("metadata");
        let persisted_indices = discover_indices(&metadata_dir, &allowed_registries)?;
        Ok(Self {
            data_dir: Arc::new(metadata_dir),
            allowed_registries: Arc::new(allowed_registries),
            freshness,
            registry,
            indices: Arc::new(RwLock::new(persisted_indices)),
            versions: Arc::default(),
            index_locks: Arc::default(),
            version_locks: Arc::default(),
            syncing: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn insert_version(&self, version: VersionMetadata) {
        let provider = ProviderKey {
            hostname: version.hostname.clone(),
            namespace: version.namespace.clone(),
            provider_type: version.provider_type.clone(),
        };
        let key = VersionKey {
            provider: provider.clone(),
            version: version.version.clone(),
        };
        self.versions.write().insert(
            key,
            CacheEntry {
                fetched_at: now_epoch(),
                value: version.clone(),
            },
        );
        let mut indices = self.indices.write();
        let entry = indices.entry(provider).or_insert_with(|| CacheEntry {
            fetched_at: now_epoch(),
            value: Vec::new(),
        });
        if !entry
            .value
            .iter()
            .any(|item| item.version == version.version)
        {
            entry.value.push(RegistryVersion {
                version: version.version,
                platforms: version
                    .platforms
                    .iter()
                    .map(|platform| crate::registry::RegistryPlatform {
                        os: platform.os.clone(),
                        arch: platform.arch.clone(),
                    })
                    .collect(),
            });
        }
    }

    pub async fn list_versions(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
    ) -> Result<Vec<String>, MetadataError> {
        let key = self.provider_key(hostname, namespace, provider_type)?;
        let entry = self.load_index(&key).await?;
        let mut versions = entry
            .value
            .into_iter()
            .map(|item| item.version)
            .collect::<Vec<_>>();
        versions.sort();
        Ok(versions)
    }

    pub async fn get_version(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
        version: &str,
    ) -> Result<Option<VersionMetadata>, MetadataError> {
        validate_component(version, "version")?;
        let provider = self.provider_key(hostname, namespace, provider_type)?;
        let key = VersionKey {
            provider,
            version: version.trim_start_matches('v').to_string(),
        };
        match self.load_version(&key).await {
            Ok(value) => Ok(Some(value.value)),
            Err(MetadataError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn get_platform_by_archive(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
        filename: &str,
    ) -> Result<Option<PlatformMetadata>, MetadataError> {
        let archive = ArchiveName::parse(provider_type, filename)
            .map_err(|_| MetadataError::InvalidAddress("archive filename".into()))?;
        Ok(self
            .get_version(hostname, namespace, provider_type, &archive.version)
            .await?
            .and_then(|metadata| {
                metadata
                    .platforms
                    .into_iter()
                    .find(|item| item.filename == filename)
            }))
    }

    pub async fn sync_known(&self) -> Result<(), MetadataError> {
        if self
            .syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(MetadataError::SyncInProgress);
        }
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.syncing);
        let keys = self.indices.read().keys().cloned().collect::<Vec<_>>();
        let mut failures = 0;
        for key in keys {
            let lock = lock_for(&self.index_locks, key.clone());
            let _guard = lock.mutex.lock().await;
            if let Err(error) = self.refresh_index_unlocked(&key).await {
                failures += 1;
                tracing::warn!(error = %error, hostname = %key.hostname, "provider metadata sync failed");
            }
        }
        if failures == 0 {
            Ok(())
        } else {
            Err(MetadataError::SyncFailed(failures))
        }
    }

    pub fn refresh_lock_count(&self) -> usize {
        self.index_locks.read().len() + self.version_locks.read().len()
    }

    fn provider_key(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
    ) -> Result<ProviderKey, MetadataError> {
        validate_hostname(hostname)?;
        validate_component(namespace, "namespace")?;
        validate_component(provider_type, "provider type")?;
        if !self.allowed_registries.contains(hostname) {
            return Err(MetadataError::RegistryNotAllowed(hostname.to_string()));
        }
        Ok(ProviderKey {
            hostname: hostname.into(),
            namespace: namespace.into(),
            provider_type: provider_type.into(),
        })
    }

    async fn load_index(
        &self,
        key: &ProviderKey,
    ) -> Result<CacheEntry<Vec<RegistryVersion>>, MetadataError> {
        if let Some(entry) = self.indices.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        let lock = lock_for(&self.index_locks, key.clone());
        let _guard = lock.mutex.lock().await;
        if let Some(entry) = self.indices.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        if !self.indices.read().contains_key(key)
            && let Some(entry) =
                read_json::<CacheEntry<Vec<RegistryVersion>>>(&self.index_path(key)).await?
        {
            self.indices.write().insert(key.clone(), entry);
        }
        if let Some(entry) = self.indices.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        match self.refresh_index_unlocked(key).await {
            Ok(entry) => Ok(entry),
            Err(error) => self.indices.read().get(key).cloned().ok_or(error),
        }
    }

    async fn refresh_index_unlocked(
        &self,
        key: &ProviderKey,
    ) -> Result<CacheEntry<Vec<RegistryVersion>>, MetadataError> {
        let versions = self
            .registry
            .versions(&key.hostname, &key.namespace, &key.provider_type)
            .await
            .map_err(map_registry)?;
        for version in &versions {
            validate_component(version.version.trim_start_matches('v'), "version")?;
            for platform in &version.platforms {
                validate_platform_component(&platform.os, "operating system")?;
                validate_platform_component(&platform.arch, "architecture")?;
            }
        }
        let entry = CacheEntry {
            fetched_at: now_epoch(),
            value: versions,
        };
        write_json_atomic(&self.index_path(key), &entry).await?;
        self.indices.write().insert(key.clone(), entry.clone());
        Ok(entry)
    }

    async fn load_version(
        &self,
        key: &VersionKey,
    ) -> Result<CacheEntry<VersionMetadata>, MetadataError> {
        if let Some(entry) = self.versions.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        let lock = lock_for(&self.version_locks, key.clone());
        let _guard = lock.mutex.lock().await;
        if let Some(entry) = self.versions.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        if !self.versions.read().contains_key(key)
            && let Some(entry) =
                read_json::<CacheEntry<VersionMetadata>>(&self.version_path(key)).await?
        {
            self.versions.write().insert(key.clone(), entry);
        }
        if let Some(entry) = self.versions.read().get(key).cloned()
            && entry.fresh(self.freshness)
        {
            return Ok(entry);
        }
        match self.refresh_version(key).await {
            Ok(entry) => Ok(entry),
            Err(error) => self.versions.read().get(key).cloned().ok_or(error),
        }
    }

    async fn refresh_version(
        &self,
        key: &VersionKey,
    ) -> Result<CacheEntry<VersionMetadata>, MetadataError> {
        let index = self.load_index(&key.provider).await?;
        let summary = index
            .value
            .into_iter()
            .find(|item| item.version.trim_start_matches('v') == key.version)
            .ok_or(MetadataError::NotFound)?;
        let platforms = stream::iter(summary.platforms.into_iter().map(|platform| {
            let registry = self.registry.clone();
            let key = key.clone();
            async move {
                let package = registry
                    .package(
                        &key.provider.hostname,
                        &key.provider.namespace,
                        &key.provider.provider_type,
                        &key.version,
                        &platform,
                    )
                    .await
                    .map_err(map_registry)?;
                validate_package(&key, &platform, &package)?;
                Ok::<_, MetadataError>(package)
            }
        }))
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
        let value = VersionMetadata {
            hostname: key.provider.hostname.clone(),
            namespace: key.provider.namespace.clone(),
            provider_type: key.provider.provider_type.clone(),
            version: key.version.clone(),
            platforms,
        };
        let entry = CacheEntry {
            fetched_at: now_epoch(),
            value,
        };
        write_json_atomic(&self.version_path(key), &entry).await?;
        self.versions.write().insert(key.clone(), entry.clone());
        Ok(entry)
    }

    fn provider_dir(&self, key: &ProviderKey) -> PathBuf {
        self.data_dir
            .join(&key.hostname)
            .join(&key.namespace)
            .join(&key.provider_type)
    }
    fn index_path(&self, key: &ProviderKey) -> PathBuf {
        self.provider_dir(key).join("index.json")
    }
    fn version_path(&self, key: &VersionKey) -> PathBuf {
        self.provider_dir(&key.provider)
            .join(format!("{}.json", key.version))
    }
}

fn discover_indices(
    root: &Path,
    allowed_registries: &HashSet<String>,
) -> Result<HashMap<ProviderKey, CacheEntry<Vec<RegistryVersion>>>, MetadataError> {
    let mut indices = HashMap::new();
    let hostnames = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(indices),
        Err(error) => return Err(error.into()),
    };
    for hostname_entry in hostnames {
        let hostname_entry = hostname_entry?;
        let hostname = hostname_entry.file_name().to_string_lossy().into_owned();
        if !allowed_registries.contains(&hostname) || !hostname_entry.file_type()?.is_dir() {
            continue;
        }
        for namespace_entry in std::fs::read_dir(hostname_entry.path())? {
            let namespace_entry = namespace_entry?;
            let namespace = namespace_entry.file_name().to_string_lossy().into_owned();
            if !valid_component(&namespace) || !namespace_entry.file_type()?.is_dir() {
                continue;
            }
            for provider_entry in std::fs::read_dir(namespace_entry.path())? {
                let provider_entry = provider_entry?;
                let provider_type = provider_entry.file_name().to_string_lossy().into_owned();
                if !valid_component(&provider_type) || !provider_entry.file_type()?.is_dir() {
                    continue;
                }
                let index_path = provider_entry.path().join("index.json");
                let bytes = match std::fs::read(index_path) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let entry = serde_json::from_slice(&bytes)?;
                indices.insert(
                    ProviderKey {
                        hostname: hostname.clone(),
                        namespace: namespace.clone(),
                        provider_type,
                    },
                    entry,
                );
            }
        }
    }
    Ok(indices)
}

struct ManagedKeyLock<K: Eq + std::hash::Hash + Clone> {
    locks: KeyLockMap<K>,
    key: K,
    mutex: Arc<Mutex<()>>,
}

impl<K: Eq + std::hash::Hash + Clone> Drop for ManagedKeyLock<K> {
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

fn lock_for<K: Eq + std::hash::Hash + Clone>(locks: &KeyLockMap<K>, key: K) -> ManagedKeyLock<K> {
    let mutex = locks
        .write()
        .entry(key.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    ManagedKeyLock {
        locks: Arc::clone(locks),
        key,
        mutex,
    }
}

fn validate_package(
    key: &VersionKey,
    requested: &crate::registry::RegistryPlatform,
    package: &PlatformMetadata,
) -> Result<(), MetadataError> {
    if package.os != requested.os || package.arch != requested.arch {
        return Err(MetadataError::InvalidAddress(
            "package platform does not match request".into(),
        ));
    }
    let archive = ArchiveName::parse(&key.provider.provider_type, &package.filename)
        .map_err(|_| MetadataError::InvalidAddress("package filename".into()))?;
    if archive.version != key.version
        || archive.os != requested.os
        || archive.arch != requested.arch
        || archive.provider_type != key.provider.provider_type
    {
        return Err(MetadataError::InvalidAddress(
            "package filename does not match request".into(),
        ));
    }
    if !package.shasum.as_deref().is_some_and(|checksum| {
        checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(MetadataError::InvalidAddress("package checksum".into()));
    }
    let url = Url::parse(&package.download_url)
        .map_err(|_| MetadataError::InvalidAddress("package download URL".into()))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(MetadataError::InvalidAddress(
            "package download URL policy".into(),
        ));
    }
    Ok(())
}

fn validate_hostname(value: &str) -> Result<(), MetadataError> {
    if value.is_empty() || value.len() > 253 || value.split('.').any(|part| !valid_component(part))
    {
        return Err(MetadataError::InvalidAddress("hostname".into()));
    }
    Ok(())
}

fn validate_component(value: &str, name: &str) -> Result<(), MetadataError> {
    if !valid_component(value.trim_start_matches('v')) {
        return Err(MetadataError::InvalidAddress(name.into()));
    }
    Ok(())
}

fn validate_platform_component(value: &str, name: &str) -> Result<(), MetadataError> {
    if value.is_empty()
        || value.len() > 32
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(MetadataError::InvalidAddress(name.into()));
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

fn map_registry(error: RegistryError) -> MetadataError {
    match error {
        RegistryError::NotFound => MetadataError::NotFound,
        error => MetadataError::Registry(error),
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, MetadataError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), MetadataError> {
    let parent = path.parent().expect("metadata paths have a parent");
    tokio::fs::create_dir_all(parent).await?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".metadata-{}-{sequence}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(value)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options.open(&temp).await?;
    use tokio::io::AsyncWriteExt;
    if let Err(error) = file.write_all(&bytes).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = tokio::fs::rename(&temp, path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    sync_parent(parent).await?;
    Ok(())
}

#[cfg(unix)]
async fn sync_parent(parent: &Path) -> Result<(), MetadataError> {
    let parent = parent.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(std::io::Error::other)??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_parent(_parent: &Path) -> Result<(), MetadataError> {
    Ok(())
}
