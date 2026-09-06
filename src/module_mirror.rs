use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use parking_lot::RwLock;
use reqwest::{Client, Url, redirect::Policy};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};

const MAX_MODULE_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MODULE_METADATA_BYTES: usize = 256 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId {
    pub hostname: String,
    pub namespace: String,
    pub name: String,
    pub system: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModule {
    pub path: PathBuf,
    pub fetched: bool,
}

#[derive(Debug, Clone)]
pub struct ModuleCache {
    root: Arc<PathBuf>,
    client: Client,
    locks: Arc<RwLock<HashMap<ModuleId, Arc<Mutex<()>>>>>,
}

impl ModuleCache {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: Arc::new(root.as_ref().to_path_buf()),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(15 * 60))
                .redirect(Policy::limited(5))
                .build()
                .expect("module download client configuration must be valid"),
            locks: Arc::default(),
        }
    }

    pub fn archive_path(&self, id: &ModuleId) -> PathBuf {
        self.root
            .join("data/modules")
            .join(&id.hostname)
            .join(&id.namespace)
            .join(&id.name)
            .join(&id.system)
            .join(format!("{}.tar.gz", id.version))
    }

    pub async fn load_or_fetch(&self, id: &ModuleId, download_url: &str) -> Result<ResolvedModule> {
        validate_module_id(id)?;
        validate_download_url(download_url)?;
        let path = self.archive_path(id);
        if self.cached_archive_is_usable(&path).await? {
            return Ok(ResolvedModule {
                path,
                fetched: false,
            });
        }

        let lock = self.lock_for(id);
        let _guard = lock.mutex.lock().await;
        if self.cached_archive_is_usable(&path).await? {
            return Ok(ResolvedModule {
                path,
                fetched: false,
            });
        }

        let parent = path.parent().context("module archive path has no parent")?;
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create module cache directory {}", parent.display()))?;

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp_path = parent.join(format!(".module-{}-{sequence}.tmp", std::process::id()));
        let mut cleanup = TempCleanup::new(tmp_path.clone());
        let response = self
            .client
            .get(download_url)
            .send()
            .await
            .with_context(|| format!("fetch module archive from {download_url}"))?
            .error_for_status()
            .with_context(|| format!("fetch module archive from {download_url}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MODULE_ARCHIVE_BYTES)
        {
            bail!("module archive exceeds maximum size");
        }
        let mut stream = response.bytes_stream();
        let mut total = 0_u64;
        let mut tmp = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .await
            .with_context(|| format!("create temp module archive {}", tmp_path.display()))?;
        while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
            let chunk = chunk.context("read module archive response body")?;
            total = total.saturating_add(chunk.len() as u64);
            if total > MAX_MODULE_ARCHIVE_BYTES {
                bail!("module archive exceeds maximum size");
            }
            tmp.write_all(&chunk)
                .await
                .with_context(|| format!("write temp module archive {}", tmp_path.display()))?;
        }
        tmp.sync_all()
            .await
            .with_context(|| format!("sync temp module archive {}", tmp_path.display()))?;
        drop(tmp);

        fs::rename(&tmp_path, &path)
            .await
            .with_context(|| format!("move module archive into cache {}", path.display()))?;

        cleanup.disarm();
        Ok(ResolvedModule {
            path,
            fetched: true,
        })
    }

    async fn cached_archive_is_usable(&self, path: &Path) -> Result<bool> {
        let metadata = match fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(metadata.file_type().is_file() && metadata.len() <= MAX_MODULE_ARCHIVE_BYTES)
    }

    fn lock_for(&self, id: &ModuleId) -> ManagedModuleLock {
        let mutex = self
            .locks
            .write()
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        ManagedModuleLock {
            locks: Arc::clone(&self.locks),
            id: id.clone(),
            mutex,
        }
    }
}

struct ManagedModuleLock {
    locks: Arc<RwLock<HashMap<ModuleId, Arc<Mutex<()>>>>>,
    id: ModuleId,
    mutex: Arc<Mutex<()>>,
}

impl Drop for ManagedModuleLock {
    fn drop(&mut self) {
        let mut locks = self.locks.write();
        if Arc::strong_count(&self.mutex) == 2
            && locks
                .get(&self.id)
                .is_some_and(|mutex| Arc::ptr_eq(mutex, &self.mutex))
        {
            locks.remove(&self.id);
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

#[derive(Debug, Clone)]
pub struct ModuleRegistryClient {
    base_url: String,
    client: Client,
}

impl ModuleRegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(Policy::none())
                .build()
                .expect("module registry client configuration must be valid"),
        }
    }

    pub async fn resolve_download_url(&self, id: &ModuleId) -> Result<String> {
        validate_module_id(id)?;
        let url = format!(
            "{}/v1/modules/{}/{}/{}/{}/download",
            self.base_url, id.namespace, id.name, id.system, id.version
        );
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("resolve module download from {url}"))?
            .error_for_status()
            .with_context(|| format!("resolve module download from {url}"))?;

        if let Some(value) = response.headers().get("X-Terraform-Get") {
            let source = value
                .to_str()
                .context("X-Terraform-Get is not valid UTF-8")?;
            return resolve_module_source(response.url(), source);
        }

        let base = response.url().clone();
        let json = read_limited_json(response).await?;
        if let Some(url) = json.get("download_url").and_then(|url| url.as_str()) {
            return resolve_module_source(&base, url);
        }

        bail!("module registry response did not include X-Terraform-Get or download_url")
    }
}

async fn read_limited_json(response: reqwest::Response) -> Result<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODULE_METADATA_BYTES as u64)
    {
        bail!("module registry response exceeds maximum size");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read module download response JSON")?;
        if body.len().saturating_add(chunk.len()) > MAX_MODULE_METADATA_BYTES {
            bail!("module registry response exceeds maximum size");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("parse module download response JSON")
}

fn resolve_module_source(base: &Url, source: &str) -> Result<String> {
    if source.starts_with("git::") {
        bail!("git module sources are not supported by the HTTP module cache")
    }
    let url = base
        .join(source)
        .context("module download source is not a valid URL")?;
    validate_download_url(url.as_str())?;
    Ok(url.to_string())
}

fn validate_download_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("module download URL is not valid")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("module download source is not a direct HTTP(S) URL")
    }
    Ok(())
}

fn validate_module_id(id: &ModuleId) -> Result<()> {
    for (name, value) in [
        ("hostname", id.hostname.as_str()),
        ("namespace", id.namespace.as_str()),
        ("module name", id.name.as_str()),
        ("system", id.system.as_str()),
        ("version", id.version.as_str()),
    ] {
        if value.is_empty()
            || value.len() > 253
            || value == "."
            || value == ".."
            || value.contains(['/', '\\'])
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+')
            })
        {
            bail!("invalid module {name}");
        }
    }
    Ok(())
}
