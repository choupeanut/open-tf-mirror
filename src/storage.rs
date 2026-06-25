use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderArchiveKey {
    pub hostname: String,
    pub namespace: String,
    pub provider_type: String,
    pub filename: String,
}

#[derive(Debug, Clone)]
pub struct ProviderStorage {
    root: Arc<PathBuf>,
}

impl ProviderStorage {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: Arc::new(root.as_ref().to_path_buf()),
        }
    }

    pub fn archive_path(&self, key: &ProviderArchiveKey) -> PathBuf {
        self.root
            .join("data/providers")
            .join(&key.hostname)
            .join(&key.namespace)
            .join(&key.provider_type)
            .join(&key.filename)
    }
}
