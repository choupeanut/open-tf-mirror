use std::{collections::HashMap, time::Duration};

use futures_util::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, de::DeserializeOwned};

use crate::metadata::PlatformMetadata;

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
    client: Client,
    origins: HashMap<String, Url>,
}

impl RegistryClient {
    pub fn new() -> Result<Self, RegistryError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            origins: HashMap::new(),
        })
    }

    pub fn with_origin(hostname: &str, origin: String) -> Result<Self, RegistryError> {
        let mut registry = Self::new()?;
        let url =
            Url::parse(&origin).map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(RegistryError::InvalidOrigin(origin));
        }
        registry.origins.insert(hostname.to_string(), url);
        Ok(registry)
    }

    fn origin(&self, hostname: &str) -> Result<Url, RegistryError> {
        if let Some(origin) = self.origins.get(hostname) {
            return Ok(origin.clone());
        }
        Url::parse(&format!("https://{hostname}"))
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))
    }

    pub async fn versions(
        &self,
        hostname: &str,
        namespace: &str,
        provider_type: &str,
    ) -> Result<Vec<RegistryVersion>, RegistryError> {
        let url = self
            .origin(hostname)?
            .join(&format!(
                "/v1/providers/{namespace}/{provider_type}/versions"
            ))
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        let response = self.client.get(url).send().await?;
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
            .origin(hostname)?
            .join(&format!(
                "/v1/providers/{namespace}/{provider_type}/{version}/download/{}/{}",
                platform.os, platform.arch
            ))
            .map_err(|error| RegistryError::InvalidOrigin(error.to_string()))?;
        let response = self.client.get(url).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound);
        }
        decode_json(response.error_for_status()?, MAX_PACKAGE_JSON_BYTES).await
    }
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
