use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformMetadata {
    pub os: String,
    pub arch: String,
    pub filename: String,
    pub shasum: Option<String>,
    pub download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMetadata {
    pub hostname: String,
    pub namespace: String,
    pub provider_type: String,
    pub version: String,
    pub platforms: Vec<PlatformMetadata>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderMetadataStore {
    versions: Arc<RwLock<HashMap<ProviderKey, HashMap<String, VersionMetadata>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    hostname: String,
    namespace: String,
    provider_type: String,
}

impl ProviderMetadataStore {
    pub fn insert_version(&self, version: VersionMetadata) {
        let key = ProviderKey {
            hostname: version.hostname.clone(),
            namespace: version.namespace.clone(),
            provider_type: version.provider_type.clone(),
        };

        self.versions
            .write()
            .entry(key)
            .or_default()
            .insert(version.version.clone(), version);
    }

    pub fn list_versions(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
    ) -> Vec<String> {
        let key = ProviderKey {
            hostname: hostname.to_string(),
            namespace: namespace.to_string(),
            provider_type: provider_type.to_string(),
        };

        let mut versions = self
            .versions
            .read()
            .get(&key)
            .map(|versions| versions.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        versions.sort();
        versions
    }

    pub fn get_version(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
        version: &str,
    ) -> Option<VersionMetadata> {
        let key = ProviderKey {
            hostname: hostname.to_string(),
            namespace: namespace.to_string(),
            provider_type: provider_type.to_string(),
        };

        self.versions
            .read()
            .get(&key)
            .and_then(|versions| versions.get(version).cloned())
    }

    pub fn get_platform_by_archive(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
        filename: &str,
    ) -> Option<PlatformMetadata> {
        let key = ProviderKey {
            hostname: hostname.to_string(),
            namespace: namespace.to_string(),
            provider_type: provider_type.to_string(),
        };

        self.versions.read().get(&key).and_then(|versions| {
            versions.values().find_map(|version| {
                version
                    .platforms
                    .iter()
                    .find(|platform| platform.filename == filename)
                    .cloned()
            })
        })
    }
}
