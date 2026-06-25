use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tokio::{fs, io::AsyncWriteExt};

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
    client: reqwest::Client,
}

impl ModuleCache {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: Arc::new(root.as_ref().to_path_buf()),
            client: reqwest::Client::new(),
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
        let path = self.archive_path(id);
        if fs::metadata(&path).await.is_ok() {
            return Ok(ResolvedModule {
                path,
                fetched: false,
            });
        }

        let parent = path.parent().context("module archive path has no parent")?;
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create module cache directory {}", parent.display()))?;

        let tmp_path = path.with_extension("tar.gz.tmp");
        let bytes = self
            .client
            .get(download_url)
            .send()
            .await
            .with_context(|| format!("fetch module archive from {download_url}"))?
            .error_for_status()
            .with_context(|| format!("fetch module archive from {download_url}"))?
            .bytes()
            .await
            .context("read module archive response body")?;

        let mut tmp = fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("create temp module archive {}", tmp_path.display()))?;
        tmp.write_all(&bytes)
            .await
            .with_context(|| format!("write temp module archive {}", tmp_path.display()))?;
        tmp.sync_all()
            .await
            .with_context(|| format!("sync temp module archive {}", tmp_path.display()))?;
        drop(tmp);

        fs::rename(&tmp_path, &path)
            .await
            .with_context(|| format!("move module archive into cache {}", path.display()))?;

        Ok(ResolvedModule {
            path,
            fetched: true,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModuleRegistryClient {
    base_url: String,
    client: reqwest::Client,
}

impl ModuleRegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn resolve_download_url(&self, id: &ModuleId) -> Result<String> {
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
            return value
                .to_str()
                .context("X-Terraform-Get is not valid UTF-8")
                .map(ToOwned::to_owned);
        }

        let json: serde_json::Value = response
            .json()
            .await
            .context("read module download response JSON")?;
        if let Some(url) = json.get("download_url").and_then(|url| url.as_str()) {
            return Ok(url.to_string());
        }

        bail!("module registry response did not include X-Terraform-Get or download_url")
    }
}
