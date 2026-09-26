use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use parking_lot::RwLock;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use crate::{
    metadata::PlatformMetadata,
    outbound::{OutboundClient, OutboundError},
};

const MAX_INDEX_JSON_BYTES: usize = 2 * 1024 * 1024;
const MAX_PACKAGE_JSON_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_VERSIONS: usize = 4096;
const MAX_PLATFORMS_PER_VERSION: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("provider was not found")]
    NotFound,
    #[error("invalid registry origin: {0}")]
    InvalidOrigin(String),
    #[error("registry request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("registry outbound policy failed: {0}")]
    Outbound(#[from] OutboundError),
    #[error("invalid registry response: {0}")]
    InvalidResponse(String),
    #[error("registry response exceeds {0} bytes")]
    ResponseTooLarge(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegistryVersion {
    pub version: String,
    pub platforms: Vec<RegistryPlatform>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegistryPlatform {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Deserialize)]
struct VersionsResponse {
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Clone)]
pub struct RegistryClient {
    outbound: OutboundClient,
    origins: HashMap<String, Url>,
    discovery_origins: HashMap<String, Url>,
    discovery_cache: Arc<RwLock<HashMap<String, DiscoveryEntry>>>,
    discovery_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
}

#[derive(Debug, Clone)]
struct DiscoveryEntry {
    origin: Url,
    fetched_at: Instant,
    retry_at: Option<Instant>,
}

#[derive(Debug, Deserialize)]
struct DiscoveryResponse {
    #[serde(rename = "providers.v1")]
    providers_v1: Option<String>,
}

impl RegistryClient {
    pub fn new() -> Result<Self, RegistryError> {
        Ok(Self {
            outbound: OutboundClient::new(crate::outbound::OutboundMode::Direct, None)?,
            origins: HashMap::new(),
            discovery_origins: HashMap::new(),
            discovery_cache: Arc::default(),
            discovery_locks: Arc::default(),
        })
    }

    pub fn with_origin(hostname: &str, origin: String) -> Result<Self, RegistryError> {
        let mut registry = Self::with_outbound(OutboundClient::for_tests());
        let url = parse_origin(&origin)?;
        registry.origins.insert(hostname.to_string(), url);
        Ok(registry)
    }

    pub fn with_discovery_origin(hostname: &str, origin: String) -> Result<Self, RegistryError> {
        let mut registry = Self::with_outbound(OutboundClient::for_tests());
        let url = parse_origin(&origin)?;
        registry.discovery_origins.insert(hostname.to_string(), url);
        Ok(registry)
    }

    pub fn with_outbound(outbound: OutboundClient) -> Self {
        Self {
            outbound,
            origins: HashMap::new(),
            discovery_origins: HashMap::new(),
            discovery_cache: Arc::default(),
            discovery_locks: Arc::default(),
        }
    }

    async fn origin(&self, hostname: &str) -> Result<Url, RegistryError> {
        if let Some(origin) = self.origins.get(hostname) {
            return Ok(origin.clone());
        }
        let base = self
            .discovery_origins
            .get(hostname)
            .cloned()
            .unwrap_or_else(|| Url::parse(&format!("https://{hostname}/")).expect("hostname URL"));
        self.discover(hostname, base).await
    }

    pub async fn versions(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
    ) -> Result<Vec<RegistryVersion>, RegistryError> {
        let url = self
            .origin(hostname)
            .await?
            .join(&format!(
                "v1/providers/{namespace}/{provider_type}/versions"
            ))
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        let response = self.outbound.get(url, Duration::from_secs(30)).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound);
        }
        let response: VersionsResponse =
            decode_json(response.error_for_status()?, MAX_INDEX_JSON_BYTES).await?;
        if response.versions.len() > MAX_PROVIDER_VERSIONS {
            return Err(RegistryError::InvalidResponse(
                "too many provider versions".into(),
            ));
        }
        if response
            .versions
            .iter()
            .any(|version| version.platforms.len() > MAX_PLATFORMS_PER_VERSION)
        {
            return Err(RegistryError::InvalidResponse(
                "too many platforms for provider version".into(),
            ));
        }
        Ok(response.versions)
    }

    pub async fn package(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
        version: &str,
        platform: &RegistryPlatform,
    ) -> Result<PlatformMetadata, RegistryError> {
        let url = self
            .origin(hostname)
            .await?
            .join(&format!(
                "v1/providers/{namespace}/{provider_type}/{version}/download/{}/{}",
                platform.os, platform.arch
            ))
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        let response = self.outbound.get(url, Duration::from_secs(30)).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound);
        }
        decode_json(response.error_for_status()?, MAX_PACKAGE_JSON_BYTES).await
    }

    async fn discover(&self, hostname: &str, base: Url) -> Result<Url, RegistryError> {
        if let Some(entry) = self.discovery_cache.read().get(hostname).cloned() {
            let age = entry.fetched_at.elapsed();
            if age < Duration::from_secs(30 * 60)
                || entry
                    .retry_at
                    .is_some_and(|retry_at| Instant::now() < retry_at)
            {
                return Ok(entry.origin);
            }
        }
        let lock = self
            .discovery_locks
            .write()
            .entry(hostname.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if let Some(entry) = self.discovery_cache.read().get(hostname).cloned() {
            let age = entry.fetched_at.elapsed();
            if age < Duration::from_secs(30 * 60)
                || entry
                    .retry_at
                    .is_some_and(|retry_at| Instant::now() < retry_at)
            {
                return Ok(entry.origin);
            }
        }
        let discovery_url = base
            .join(".well-known/terraform.json")
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        let result = async {
            let response = self
                .outbound
                .get(discovery_url, Duration::from_secs(30))
                .await?;
            let document =
                decode_json::<DiscoveryResponse>(response.error_for_status()?, 64 * 1024).await?;
            let service = document.providers_v1.ok_or_else(|| {
                RegistryError::InvalidResponse(
                    "Terraform discovery document is missing providers.v1".into(),
                )
            })?;
            let origin = base
                .join(&service)
                .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
            if origin.host_str().is_none()
                || (!self.outbound.allows_http_for_tests() && origin.scheme() != "https")
            {
                return Err(RegistryError::InvalidOrigin(
                    "providers.v1 must be an HTTPS URL with a host".into(),
                ));
            }
            Ok::<_, RegistryError>(normalize_origin(origin))
        }
        .await;
        match result {
            Ok(origin) => {
                self.discovery_cache.write().insert(
                    hostname.to_string(),
                    DiscoveryEntry {
                        origin,
                        fetched_at: Instant::now(),
                        retry_at: None,
                    },
                );
                Ok(self
                    .discovery_cache
                    .read()
                    .get(hostname)
                    .expect("discovery cache entry inserted")
                    .origin
                    .clone())
            }
            Err(error) => {
                if let Some(entry) = self.discovery_cache.write().get_mut(hostname) {
                    entry.retry_at = Some(Instant::now() + Duration::from_secs(30));
                    return Ok(entry.origin.clone());
                }
                Err(error)
            }
        }
    }
}

fn parse_origin(origin: &str) -> Result<Url, RegistryError> {
    let url =
        Url::parse(origin).map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(RegistryError::InvalidOrigin(origin.to_string()));
    }
    Ok(normalize_origin(url))
}

fn normalize_origin(mut origin: Url) -> Url {
    if !origin.path().ends_with('/') {
        let path = format!("{}/", origin.path());
        origin.set_path(&path);
    }
    origin
}

async fn decode_json<T: DeserializeOwned>(
    response: reqwest::Response,
    maximum: usize,
) -> Result<T, RegistryError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(RegistryError::ResponseTooLarge(maximum));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(RegistryError::ResponseTooLarge(maximum));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|error| RegistryError::InvalidResponse(error.to_string()))
}
