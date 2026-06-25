use std::{collections::BTreeMap, path::Path};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;

use crate::{
    metadata::ProviderMetadataStore,
    module_mirror::{ModuleCache, ModuleId, ModuleRegistryClient},
    provider::ArchiveName,
    storage::{ProviderArchiveKey, ProviderStorage},
};

#[derive(Debug, Clone)]
pub struct AppState {
    pub metadata: ProviderMetadataStore,
    pub provider_storage: ProviderStorage,
    pub module_cache: ModuleCache,
    pub module_registry_base: String,
}

impl AppState {
    pub fn for_tests(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            metadata: ProviderMetadataStore::default(),
            provider_storage: ProviderStorage::new(root),
            module_cache: ModuleCache::new(root),
            module_registry_base: "https://registry.terraform.io".to_string(),
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/readyz", get(healthz))
        .route("/livez", get(healthz))
        .route(
            "/v1/providers/:hostname/:namespace/:provider_type/:action",
            get(get_provider_metadata),
        )
        .route(
            "/v1/providers/:hostname/:namespace/:provider_type/download/:archive",
            get(download_provider_archive),
        )
        .route(
            "/v1/modules/:namespace/:name/:system/:version/download",
            get(download_module_archive),
        )
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Serialize)]
struct VersionsResponse {
    versions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct VersionArchivesResponse {
    archives: BTreeMap<String, ArchiveResponse>,
}

#[derive(Debug, Serialize)]
struct ArchiveResponse {
    url: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hashes: Vec<String>,
}

async fn get_provider_metadata(
    State(state): State<AppState>,
    AxumPath((hostname, namespace, provider_type, action)): AxumPath<(
        String,
        String,
        String,
        String,
    )>,
) -> impl IntoResponse {
    if action.len() <= 5 {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let version = action[..action.len() - 5].trim_start_matches('v');
    if version == "index" {
        let versions = state
            .metadata
            .list_versions(&hostname, &namespace, &provider_type)
            .into_iter()
            .map(|version| (version, serde_json::json!({})))
            .collect();
        return Json(VersionsResponse { versions }).into_response();
    }

    let Some(metadata) = state
        .metadata
        .get_version(&hostname, &namespace, &provider_type, version)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let archives = metadata
        .platforms
        .into_iter()
        .map(|platform| {
            let key = format!("{}_{}", platform.os, platform.arch);
            let hashes = platform
                .shasum
                .map(|shasum| vec![format!("zh:{shasum}")])
                .unwrap_or_default();
            (
                key,
                ArchiveResponse {
                    url: format!("download/{}", platform.filename),
                    hashes,
                },
            )
        })
        .collect();

    Json(VersionArchivesResponse { archives }).into_response()
}

async fn download_provider_archive(
    State(state): State<AppState>,
    AxumPath((hostname, namespace, provider_type, archive)): AxumPath<(
        String,
        String,
        String,
        String,
    )>,
) -> impl IntoResponse {
    if ArchiveName::parse(&provider_type, &archive).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if state
        .metadata
        .get_platform_by_archive(&hostname, &namespace, &provider_type, &archive)
        .is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    let key = ProviderArchiveKey {
        hostname,
        namespace,
        provider_type,
        filename: archive.clone(),
    };
    let path = state.provider_storage.archive_path(&key);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mut response = Body::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    match HeaderValue::from_str(&format!("attachment; filename=\"{archive}\"")) {
        Ok(value) => {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    response
}

async fn download_module_archive(
    State(state): State<AppState>,
    AxumPath((namespace, name, system, version)): AxumPath<(String, String, String, String)>,
) -> impl IntoResponse {
    let id = ModuleId {
        hostname: "registry.terraform.io".to_string(),
        namespace,
        name,
        system,
        version,
    };
    let registry = ModuleRegistryClient::new(&state.module_registry_base);
    let download_url = match registry.resolve_download_url(&id).await {
        Ok(url) => url,
        Err(err) => {
            tracing::warn!(error = %err, "failed to resolve module download URL");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let resolved = match state.module_cache.load_or_fetch(&id, &download_url).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load module archive");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let bytes = match tokio::fs::read(&resolved.path).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mut response = Body::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    response
}
