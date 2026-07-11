use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, put},
};
use parking_lot::Mutex;
use serde::Serialize;
use tokio_util::io::ReaderStream;

use crate::{
    metadata::{MetadataError, ProviderMetadataStore},
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
    pub data_dir: Arc<PathBuf>,
}

impl AppState {
    pub fn for_tests(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            metadata: ProviderMetadataStore::new(
                root,
                std::collections::HashSet::from(["registry.terraform.io".to_string()]),
                std::time::Duration::from_secs(30 * 60),
            )
            .expect("test metadata service configuration must be valid"),
            provider_storage: ProviderStorage::new(root),
            module_cache: ModuleCache::new(root),
            module_registry_base: "https://registry.terraform.io".to_string(),
            data_dir: Arc::new(root.to_path_buf()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouterOptions {
    pub enable_module_mirror: bool,
    pub conn_qps: u32,
    pub conn_burst: u32,
}

impl Default for RouterOptions {
    fn default() -> Self {
        Self {
            enable_module_mirror: false,
            conn_qps: 100,
            conn_burst: 200,
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    build_router_with_options(state, RouterOptions::default())
}

pub fn build_router_with_options(state: AppState, options: RouterOptions) -> Router {
    let limiter = TokenBucket::new(options.conn_qps, options.conn_burst);
    let provider_routes = Router::new()
        .route(
            "/v1/providers/:hostname/:namespace/:provider_type/:action",
            get(get_provider_metadata),
        )
        .route(
            "/v1/providers/:hostname/:namespace/:provider_type/download/:archive",
            get(download_provider_archive),
        )
        .route("/v1/providers/sync", put(sync_providers))
        .layer(middleware::from_fn_with_state(
            limiter.clone(),
            enforce_rate_limit,
        ));
    let mut router = Router::new()
        .route("/readyz", get(readyz))
        .route("/livez", get(livez))
        .merge(provider_routes);
    if options.enable_module_mirror {
        router = router.merge(
            Router::new()
                .route(
                    "/v1/modules/:namespace/:name/:system/:version/download",
                    get(download_module_archive),
                )
                .layer(middleware::from_fn_with_state(limiter, enforce_rate_limit)),
        );
    }
    router.with_state(state)
}

async fn livez() -> &'static str {
    "ok"
}

static READY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let sequence = READY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let probe = state
        .data_dir
        .join(format!(".readyz-{}-{sequence}", std::process::id()));
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let file = options.open(&probe).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::remove_file(&probe).await
    }
    .await;
    match result {
        Ok(()) => (StatusCode::OK, "ok"),
        Err(error) => {
            tracing::warn!(error = %error, data_dir = %state.data_dir.display(), "cache readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "cache unavailable")
        }
    }
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

#[derive(Debug, Clone)]
struct TokenBucket {
    qps: f64,
    burst: f64,
    state: Arc<Mutex<BucketState>>,
}

impl TokenBucket {
    fn new(qps: u32, burst: u32) -> Self {
        let burst = burst.max(1) as f64;
        Self {
            qps: qps.max(1) as f64,
            burst,
            state: Arc::new(Mutex::new(BucketState {
                tokens: burst,
                last_refill: Instant::now(),
            })),
        }
    }

    fn try_acquire(&self) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock();
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.qps).min(self.burst);
        state.last_refill = now;
        if state.tokens < 1.0 {
            return false;
        }
        state.tokens -= 1.0;
        true
    }
}

async fn enforce_rate_limit(
    State(limiter): State<TokenBucket>,
    request: Request<Body>,
    next: Next,
) -> axum::response::Response {
    if limiter.try_acquire() {
        return next.run(request).await;
    }
    let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
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
    let Some(version) = action.strip_suffix(".json").map(str::trim) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let version = version.trim_start_matches('v');
    if version == "index" {
        let versions = match state
            .metadata
            .list_versions(&hostname, &namespace, &provider_type)
            .await
        {
            Ok(versions) => versions,
            Err(error) => return metadata_error_response(error),
        }
        .into_iter()
        .map(|version| (version, serde_json::json!({})))
        .collect();
        return Json(VersionsResponse { versions }).into_response();
    }

    let metadata = match state
        .metadata
        .get_version(&hostname, &namespace, &provider_type, version)
        .await
    {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return metadata_error_response(error),
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

    let platform = match state
        .metadata
        .get_platform_by_archive(&hostname, &namespace, &provider_type, &archive)
        .await
    {
        Ok(Some(platform)) => platform,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return metadata_error_response(error),
    };
    if platform.filename != archive {
        return StatusCode::NOT_FOUND.into_response();
    }

    let key = ProviderArchiveKey {
        hostname,
        namespace,
        provider_type,
        filename: archive.clone(),
    };
    let path = match state.provider_storage.load_or_fetch(&key, &platform).await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load provider archive");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
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

async fn sync_providers(State(state): State<AppState>) -> impl IntoResponse {
    match state.metadata.sync_known().await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(MetadataError::SyncInProgress) => StatusCode::CONFLICT,
        Err(error) => {
            tracing::warn!(error = %error, "provider metadata synchronization failed");
            StatusCode::BAD_GATEWAY
        }
    }
}

fn metadata_error_response(error: MetadataError) -> axum::response::Response {
    match error {
        MetadataError::NotFound => StatusCode::NOT_FOUND.into_response(),
        MetadataError::InvalidAddress(_) | MetadataError::RegistryNotAllowed(_) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        MetadataError::SyncInProgress => StatusCode::CONFLICT.into_response(),
        error => {
            tracing::warn!(error = %error, "provider metadata request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
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

    let file = match tokio::fs::File::open(&resolved.path).await {
        Ok(file) => file,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    response
}
